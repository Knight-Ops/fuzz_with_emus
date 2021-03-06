//! A 64-bit RISC-V RV64i interpreter

use std::fmt;
use std::mem::size_of_val;
use std::sync::Arc;
use std::path::Path;
use std::time::Duration;
use std::process::Command;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use crate::rdtsc;
use crate::Corpus;
use crate::mmu::{VirtAddr, Perm, PERM_READ, PERM_WRITE, PERM_EXEC, PERM_RAW};
use crate::mmu::{Mmu, DIRTY_BLOCK_SIZE};
use crate::jitcache::JitCache;

/// If set, all register state will be saved before the execution of every
/// instruction.
/// This is INCREDIBLY slow and should only be used for debugging
const ENABLE_TRACING: bool = false;

/// Make sure this stays in sync with the C++ JIT version of this structure
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    None,
    IndirectBranch,
    ReadFault,
    WriteFault,
    Ecall,
    Ebreak,
    Timeout,
    Breakpoint,
    InvalidOpcode,
    Coverage,
}

/// Make sure this stays in sync with the C++ JIT version of this structure
#[repr(C)]
#[derive(Clone, Copy)]
struct GuestState {
    exit_reason:   ExitReason,
    reenter_pc:    u64,
    cov_from:      u64,
    cov_to:        u64,
    regs:          [u64; 33],
    memory:        usize,
    permissions:   usize,
    dirty:         usize,
    dirty_idx:     usize,
    dirty_bitmap:  usize,
    trace_buffer:  usize,
    trace_idx:     usize,
    trace_len:     usize,
    cov_bitmap:    usize,
    instrs_execed: u64,
    timeout:       u64,
}

impl Default for GuestState {
    fn default() -> Self {
        GuestState {
            exit_reason:   ExitReason::None,
            reenter_pc:    0,
            cov_from:      0,
            cov_to:        0,
            regs:          [0; 33],
            memory:        0,
            permissions:   0,
            dirty:         0,
            dirty_idx:     0,
            dirty_bitmap:  0,
            trace_buffer:  0,
            trace_idx:     0,
            trace_len:     0,
            cov_bitmap:    0,
            instrs_execed: 0,
            timeout:       50_000_000,
        }
    }
}

/// An R-type instruction
#[derive(Debug)]
struct Rtype {
    funct7: u32,
    rs2:    Register,
    rs1:    Register,
    funct3: u32,
    rd:     Register,
}

impl From<u32> for Rtype {
    fn from(inst: u32) -> Self {
        Rtype {
            funct7: (inst >> 25) & 0b1111111,
            rs2:    Register::from((inst >> 20) & 0b11111),
            rs1:    Register::from((inst >> 15) & 0b11111),
            funct3: (inst >> 12) & 0b111,
            rd:     Register::from((inst >>  7) & 0b11111),
        }
    }
}

/// An S-type instruction
#[derive(Debug)]
struct Stype {
    imm:    i32,
    rs2:    Register,
    rs1:    Register,
    funct3: u32,
}

impl From<u32> for Stype {
    fn from(inst: u32) -> Self {
        let imm115 = (inst >> 25) & 0b1111111;
        let imm40  = (inst >>  7) & 0b11111;

        let imm = (imm115 << 5) | imm40;
        let imm = ((imm as i32) << 20) >> 20;

        Stype {
            imm:    imm,
            rs2:    Register::from((inst >> 20) & 0b11111),
            rs1:    Register::from((inst >> 15) & 0b11111),
            funct3: (inst >> 12) & 0b111,
        }
    }
}

/// A J-type instruction
#[derive(Debug)]
struct Jtype {
    imm: i32,
    rd:  Register,
}

impl From<u32> for Jtype {
    fn from(inst: u32) -> Self {
        let imm20   = (inst >> 31) & 1;
        let imm101  = (inst >> 21) & 0b1111111111;
        let imm11   = (inst >> 20) & 1;
        let imm1912 = (inst >> 12) & 0b11111111;

        let imm = (imm20 << 20) | (imm1912 << 12) | (imm11 << 11) |
            (imm101 << 1);
        let imm = ((imm as i32) << 11) >> 11;

        Jtype {
            imm: imm,
            rd:  Register::from((inst >> 7) & 0b11111),
        }
    }
}

/// A B-type instruction
#[derive(Debug)]
struct Btype {
    imm:    i32,
    rs2:    Register,
    rs1:    Register,
    funct3: u32,
}

impl From<u32> for Btype {
    fn from(inst: u32) -> Self {
        let imm12  = (inst >> 31) & 1;
        let imm105 = (inst >> 25) & 0b111111;
        let imm41  = (inst >>  8) & 0b1111;
        let imm11  = (inst >>  7) & 1;

        let imm = (imm12 << 12) | (imm11 << 11) |(imm105 << 5) | (imm41 << 1);
        let imm = ((imm as i32) << 19) >> 19;

        Btype {
            imm:    imm,
            rs2:    Register::from((inst >> 20) & 0b11111),
            rs1:    Register::from((inst >> 15) & 0b11111),
            funct3: (inst >> 12) & 0b111,
        }
    }
}

/// An I-type instruction
#[derive(Debug)]
struct Itype {
    imm:    i32,
    rs1:    Register,
    funct3: u32,
    rd:     Register,
}

impl From<u32> for Itype {
    fn from(inst: u32) -> Self {
        let imm = (inst as i32) >> 20;
        Itype {
            imm:    imm,
            rs1:    Register::from((inst >> 15) & 0b11111),
            funct3: (inst >> 12) & 0b111,
            rd:     Register::from((inst >>  7) & 0b11111),
        }
    }
}

#[derive(Debug)]
struct Utype {
    imm: i32,
    rd:  Register,
}

impl From<u32> for Utype {
    fn from(inst: u32) -> Self {
        Utype {
            imm: (inst & !0xfff) as i32,
            rd:  Register::from((inst >> 7) & 0b11111),
        }
    }
}

/// An open file
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmuFile {
    Stdin,
    Stdout,
    Stderr,

    // A file which is backed by the current fuzz input
    FuzzInput { cursor: usize },
}

/// A list of all open files
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Files(Vec<Option<EmuFile>>);

impl Files {
    /// Get access to a file descriptor for `fd`
    pub fn get_file(&mut self, fd: usize) -> Option<&mut Option<EmuFile>> {
        self.0.get_mut(fd)
    }
}

/// Callback for breakpoints
type BreakpointCallback = fn(&mut Emulator) -> Result<(), VmExit>;

/// All the state of the emulated system
pub struct Emulator {
    /// Memory for the emulator
    pub memory: Mmu,

    /// All RV64i registers
    state: GuestState,

    /// Fuzz input for the program
    pub fuzz_input: Vec<u8>,

    /// File handle table (indexed by file descriptor)
    pub files: Files,

    /// Breakpoint callbacks
    breakpoints: BTreeMap<VirtAddr, BreakpointCallback>,

    /// JIT cache, if we are using a JIT
    jit_cache: Option<Arc<JitCache>>,

    /// Trace of register states prior to every instruction execution
    /// Only allocated if `ENABLE_TRACING` is `true`
    trace: Vec<[u64; 33]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Reasons why the VM exited
pub enum VmExit {
    /// The VM exited due to a syscall instruction
    Syscall,

    /// The VM exited cleanly as requested by the VM
    Exit,

    /// A RISC-V software breakpoint instruction was hit
    Ebreak,

    /// The instruction count limit was hit and a timeout has occurred
    Timeout,

    /// An invalid opcode was lifted
    InvalidOpcode,

    /// A free of an invalid region was performed
    InvalidFree(VirtAddr),

    /// An integer overflow occured during a syscall due to bad supplied
    /// arguments by the program
    SyscallIntegerOverflow,

    /// A read or write memory request overflowed the address size
    AddressIntegerOverflow,

    /// The address requested was not in bounds of the guest memory space
    AddressMiss(VirtAddr, usize),

    /// An read of `VirtAddr` failed due to missing permissions
    ReadFault(VirtAddr),

    /// An execution of a `VirtAddr` failed
    ExecFault(VirtAddr),

    /// A read of memory which is uninitialized, but otherwise readable failed
    /// at `VirtAddr`
    UninitFault(VirtAddr),
    
    /// An write of `VirtAddr` failed due to missing permissions
    WriteFault(VirtAddr),
}

/// Different types of faults
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultType {
    // Access occurred outside of program memory
    Bounds,

    // Invalid free (eg, double free or corrupt free address)
    Free,

    // An invalid opcode was executed (or lifted)
    InvalidOpcode,

    Exec,
    Read,
    Write,
    Uninit,
}

/// Different buckets for addresses
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressType {
    /// Address was between [0, 32 KiB)
    Null,

    /// Address was between [-32 KiB, 0)
    Negative,

    /// Address was normal
    Normal,
}

impl From<VirtAddr> for AddressType {
    fn from(val: VirtAddr) -> Self {
        match val.0 as i64 {
            (0..=32767)   => AddressType::Null,
            (-32768..=-1) => AddressType::Negative,
            _ => AddressType::Normal,
        }
    }
}

impl VmExit {
    /// If this is a crash it returns the faulting address and the fault type
    pub fn is_crash(&self) -> Option<(FaultType, VirtAddr)> {
        match *self {
            VmExit::AddressMiss(addr, _) => Some((FaultType::Bounds, addr)),
            VmExit::ReadFault(addr)      => Some((FaultType::Read,   addr)),
            VmExit::ExecFault(addr)      => Some((FaultType::Exec,   addr)),
            VmExit::UninitFault(addr)    => Some((FaultType::Uninit, addr)),
            VmExit::WriteFault(addr)     => Some((FaultType::Write,  addr)),
            VmExit::InvalidFree(addr)    => Some((FaultType::Free,   addr)),
            VmExit::InvalidOpcode =>
                Some((FaultType::InvalidOpcode, VirtAddr(0))),
            _ => None,
        }
    }
}

impl fmt::Display for Emulator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,
r#"zero {:016x} ra {:016x} sp  {:016x} gp  {:016x}
tp   {:016x} t0 {:016x} t1  {:016x} t2  {:016x}
s0   {:016x} s1 {:016x} a0  {:016x} a1  {:016x}
a2   {:016x} a3 {:016x} a4  {:016x} a5  {:016x}
a6   {:016x} a7 {:016x} s2  {:016x} s3  {:016x}
s4   {:016x} s5 {:016x} s6  {:016x} s7  {:016x}
s8   {:016x} s9 {:016x} s10 {:016x} s11 {:016x}
t3   {:016x} t4 {:016x} t5  {:016x} t6  {:016x}
pc   {:016x}"#,
        self.reg(Register::Zero),
        self.reg(Register::Ra),
        self.reg(Register::Sp),
        self.reg(Register::Gp),
        self.reg(Register::Tp),
        self.reg(Register::T0),
        self.reg(Register::T1),
        self.reg(Register::T2),
        self.reg(Register::S0),
        self.reg(Register::S1),
        self.reg(Register::A0),
        self.reg(Register::A1),
        self.reg(Register::A2),
        self.reg(Register::A3),
        self.reg(Register::A4),
        self.reg(Register::A5),
        self.reg(Register::A6),
        self.reg(Register::A7),
        self.reg(Register::S2),
        self.reg(Register::S3),
        self.reg(Register::S4),
        self.reg(Register::S5),
        self.reg(Register::S6),
        self.reg(Register::S7),
        self.reg(Register::S8),
        self.reg(Register::S9),
        self.reg(Register::S10),
        self.reg(Register::S11),
        self.reg(Register::T3),
        self.reg(Register::T4),
        self.reg(Register::T5),
        self.reg(Register::T6),
        self.reg(Register::Pc))
    }
}

/// 64-bit RISC-V registers
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Register {
    Zero = 0,
    Ra,
    Sp,
    Gp,
    Tp,
    T0,
    T1,
    T2,
    S0,
    S1,
    A0,
    A1,
    A2,
    A3,
    A4,
    A5,
    A6,
    A7,
    S2,
    S3,
    S4,
    S5,
    S6,
    S7,
    S8,
    S9,
    S10,
    S11,
    T3,
    T4,
    T5,
    T6,
    Pc,
}

impl From<u32> for Register {
    fn from(val: u32) -> Self {
        assert!(val < 33);
        unsafe {
            core::ptr::read_unaligned(&(val as usize) as
                                      *const usize as *const Register)
        }
    }
}

impl Emulator {
    /// Creates a new emulator with `size` bytes of memory
    pub fn new(size: usize) -> Self {
        assert!(size >= 8, "Must have at least 8 bytes of memory");

        Emulator {
            memory: Mmu::new(size),
            state:  GuestState::default(),
            fuzz_input: Vec::new(),
            files: Files(vec![
                Some(EmuFile::Stdin),
                Some(EmuFile::Stdout),
                Some(EmuFile::Stderr),
            ]),
            jit_cache: None,
            breakpoints: BTreeMap::new(),
            trace: Vec::with_capacity(
                if ENABLE_TRACING { 10_000_000 } else { 0 }),
        }
    }

    /// Fork an emulator into a new emulator which will diff from the original
    pub fn fork(&self) -> Self {
        let mut state = GuestState::default();
        state.regs = self.state.regs;

        Emulator {
            memory:      self.memory.fork(),
            state:       state,
            fuzz_input:  self.fuzz_input.clone(),
            files:       self.files.clone(),
            jit_cache:   self.jit_cache.clone(),
            breakpoints: self.breakpoints.clone(),
            trace: Vec::with_capacity(
                if ENABLE_TRACING { 10_000_000 } else { 0 }),
        }
    }

    /// Enable the JIT and use a specified `JitCache`
    pub fn enable_jit(mut self, jit_cache: Arc<JitCache>) -> Self {
        self.jit_cache = Some(jit_cache);
        self
    }
    
    /// Register a new breakpoint callback
    pub fn add_breakpoint(&mut self, pc: VirtAddr,
                          callback: BreakpointCallback) {
        self.breakpoints.insert(pc, callback);
    }

    /// Reset the state of `self` to `other`, assuming that `self` is
    /// forked off of `other`. If it is not, the results are invalid.
    pub fn reset(&mut self, other: &Self) {
        if ENABLE_TRACING {
            let mut tracestr = String::new();
            let mut pctracestr = String::new();
            for trace in &self.trace {
                self.state.regs = *trace;
                tracestr += &format!("{}\n", self);
                pctracestr += &format!("{:x}\n", self.reg(Register::Pc));
            }
            if self.trace.len() > 0 {
                std::fs::write("trace.txt", tracestr).unwrap();
                std::fs::write("pctrace.txt", pctracestr).unwrap();
                panic!();
            }
        
            // Reset trace state
            self.trace.clear();
        }

        // Reset memory state
        self.memory.reset(&other.memory);

        // Reset register state
        self.state.regs = other.state.regs;

        // Reset file state
        self.files.0.clear();
        self.files.0.extend_from_slice(&other.files.0);
    }

    /// Allocate a new file descriptor
    pub fn alloc_file(&mut self) -> usize {
        for (fd, file) in self.files.0.iter().enumerate() {
            if file.is_none() {
                // File not present, we can reuse the FD
                return fd;
            }
        }
        
        // If we got here, no FD is present, create a new one
        let fd = self.files.0.len();
        self.files.0.push(None);
        fd
    }

    /// Get a register from the guest
    pub fn reg(&self, register: Register) -> u64 {
        if register != Register::Zero {
            self.state.regs[register as usize]
        } else {
            0
        }
    }
    
    /// Set a register in the guest
    pub fn set_reg(&mut self, register: Register, val: u64) {
        if register != Register::Zero {
            self.state.regs[register as usize] = val;
        }
    }

    /// Run the VM using either the emulator or the JIT
    pub fn run(&mut self, instrs_execed: &mut u64,
               vm_cycles: &mut u64, corpus: &Corpus)
            -> Result<(), VmExit> {
        if self.jit_cache.is_some() {
            self.run_jit(instrs_execed, vm_cycles, corpus)
        } else {
            let it = rdtsc();
            let ret = self.run_emu(instrs_execed, corpus);
            *vm_cycles += rdtsc() - it;
            ret
        }
    }

    /// Run the VM using the emulator
    pub fn run_emu(&mut self, instrs_execed: &mut u64, corpus: &Corpus)
            -> Result<(), VmExit> {
        'next_inst: loop {
            // Get the current program counter
            let pc = self.reg(Register::Pc);
            
            // Check alignment
            if pc & 3 != 0 {
                // Code was unaligned, return a code fetch fault
                return Err(VmExit::ExecFault(VirtAddr(pc as usize)));
            }

            // Read the instruction
            let inst: u32 = self.memory.read_perms(VirtAddr(pc as usize), 
                                                   Perm(PERM_EXEC))
                .map_err(|x| VmExit::ExecFault(x.is_crash().unwrap().1))?;

            //print!("Executing {:#x}\n", pc);
            if ENABLE_TRACING {
                self.trace.push(self.state.regs);
            }
           
            if let Some(callback) =
                    self.breakpoints.get(&VirtAddr(pc as usize)) {
                // Invoke the breakpoint callback
                callback(self)?;

                if self.reg(Register::Pc) != pc {
                    // Callback changed PC, re-start emulation loop
                    continue 'next_inst;
                }
            }

            // Update number of instructions executed
            *instrs_execed += 1;

            // Extract the opcode from the instruction
            let opcode = inst & 0b1111111;

            //print!("{}\n\n", self);

            match opcode {
                0b0110111 => {
                    // LUI
                    let inst = Utype::from(inst);
                    self.set_reg(inst.rd, inst.imm as i64 as u64);
                }
                0b0010111 => {
                    // AUIPC
                    let inst = Utype::from(inst);
                    self.set_reg(inst.rd,
                                 (inst.imm as i64 as u64).wrapping_add(pc));
                }
                0b1101111 => {
                    // JAL
                    let inst = Jtype::from(inst);
                    self.set_reg(inst.rd, pc.wrapping_add(4));
                    self.set_reg(Register::Pc,
                                 pc.wrapping_add(inst.imm as i64 as u64));
                    continue 'next_inst;
                }
                0b1100111 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // JALR
                            let target = self.reg(inst.rs1).wrapping_add(
                                    inst.imm as i64 as u64);
                            self.set_reg(inst.rd, pc.wrapping_add(4));
                            self.set_reg(Register::Pc, target);
                            continue 'next_inst;
                        }
                        _ => unimplemented!("Unexpected 0b1100111"),
                    }
                }
                0b1100011 => {
                    // We know it's an Btype
                    let inst = Btype::from(inst);

                    let rs1 = self.reg(inst.rs1);
                    let rs2 = self.reg(inst.rs2);

                    match inst.funct3 {
                        0b000 => {
                            // BEQ
                            if rs1 == rs2 {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b001 => {
                            // BNE
                            if rs1 != rs2 {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b100 => {
                            // BLT
                            if (rs1 as i64) < (rs2 as i64) {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b101 => {
                            // BGE
                            if (rs1 as i64) >= (rs2 as i64) {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b110 => {
                            // BLTU
                            if (rs1 as u64) < (rs2 as u64) {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        0b111 => {
                            // BGEU
                            if (rs1 as u64) >= (rs2 as u64) {
                                self.set_reg(Register::Pc,
                                    pc.wrapping_add(inst.imm as i64 as u64));
                                continue 'next_inst;
                            }
                        }
                        _ => unimplemented!("Unexpected 0b1100011"),
                    }
                }
                0b0000011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);

                    // Compute the address
                    let addr = VirtAddr(self.reg(inst.rs1)
                        .wrapping_add(inst.imm as i64 as u64)
                        as usize);

                    match inst.funct3 {
                        0b000 => {
                            // LB
                            let mut tmp = [0u8; 1];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                i8::from_le_bytes(tmp) as i64 as u64);
                        }
                        0b001 => {
                            // LH
                            let mut tmp = [0u8; 2];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                i16::from_le_bytes(tmp) as i64 as u64);
                        }
                        0b010 => {
                            // LW
                            let mut tmp = [0u8; 4];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                i32::from_le_bytes(tmp) as i64 as u64);
                        }
                        0b011 => {
                            // LD
                            let mut tmp = [0u8; 8];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                i64::from_le_bytes(tmp) as i64 as u64);
                        }
                        0b100 => {
                            // LBU
                            let mut tmp = [0u8; 1];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                u8::from_le_bytes(tmp) as u64);
                        }
                        0b101 => {
                            // LHU
                            let mut tmp = [0u8; 2];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                u16::from_le_bytes(tmp) as u64);
                        }
                        0b110 => {
                            // LWU
                            let mut tmp = [0u8; 4];
                            self.memory.read_into(addr, &mut tmp)?;
                            self.set_reg(inst.rd,
                                u32::from_le_bytes(tmp) as u64);
                        }
                        _ => unimplemented!("Unexpected 0b0000011"),
                    }
                }
                0b0100011 => {
                    // We know it's an Stype
                    let inst = Stype::from(inst);

                    // Compute the address
                    let addr = VirtAddr(self.reg(inst.rs1)
                        .wrapping_add(inst.imm as i64 as u64)
                        as usize);

                    match inst.funct3 {
                        0b000 => {
                            // SB
                            let val = self.reg(inst.rs2) as u8;
                            self.memory.write(addr, val)?;
                        }
                        0b001 => {
                            // SH
                            let val = self.reg(inst.rs2) as u16;
                            self.memory.write(addr, val)?;
                        }
                        0b010 => {
                            // SW
                            let val = self.reg(inst.rs2) as u32;
                            self.memory.write(addr, val)?;
                        }
                        0b011 => {
                            // SD
                            let val = self.reg(inst.rs2) as u64;
                            self.memory.write(addr, val)?;
                        }
                        _ => unimplemented!("Unexpected 0b0100011"),
                    }
                }
                0b0010011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);
                    
                    let rs1 = self.reg(inst.rs1);
                    let imm = inst.imm as i64 as u64;

                    match inst.funct3 {
                        0b000 => {
                            // ADDI
                            self.set_reg(inst.rd, rs1.wrapping_add(imm));
                        }
                        0b010 => {
                            // SLTI
                            if (rs1 as i64) < (imm as i64) {
                                self.set_reg(inst.rd, 1);
                            } else {
                                self.set_reg(inst.rd, 0);
                            }
                        }
                        0b011 => {
                            // SLTIU
                            if (rs1 as u64) < (imm as u64) {
                                self.set_reg(inst.rd, 1);
                            } else {
                                self.set_reg(inst.rd, 0);
                            }
                        }
                        0b100 => {
                            // XORI
                            self.set_reg(inst.rd, rs1 ^ imm);
                        }
                        0b110 => {
                            // ORI
                            self.set_reg(inst.rd, rs1 | imm);
                        }
                        0b111 => {
                            // ANDI
                            self.set_reg(inst.rd, rs1 & imm);
                        }
                        0b001 => {
                            let mode = (inst.imm >> 6) & 0b111111;
                            
                            match mode {
                                0b000000 => {
                                    // SLLI
                                    let shamt = inst.imm & 0b111111;
                                    self.set_reg(inst.rd, rs1 << shamt);
                                }
                                _ => unreachable!(),
                            }
                        }
                        0b101 => {
                            let mode = (inst.imm >> 6) & 0b111111;
                            
                            match mode {
                                0b000000 => {
                                    // SRLI
                                    let shamt = inst.imm & 0b111111;
                                    self.set_reg(inst.rd, rs1 >> shamt);
                                }
                                0b010000 => {
                                    // SRAI
                                    let shamt = inst.imm & 0b111111;
                                    self.set_reg(inst.rd,
                                        ((rs1 as i64) >> shamt) as u64);
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                0b0110011 => {
                    // We know it's an Rtype
                    let inst = Rtype::from(inst);

                    let rs1 = self.reg(inst.rs1);
                    let rs2 = self.reg(inst.rs2);

                    match (inst.funct7, inst.funct3) {
                        (0b0000000, 0b000) => {
                            // ADD
                            self.set_reg(inst.rd, rs1.wrapping_add(rs2));
                        }
                        (0b0100000, 0b000) => {
                            // SUB
                            self.set_reg(inst.rd, rs1.wrapping_sub(rs2));
                        }
                        (0b0000000, 0b001) => {
                            // SLL
                            let shamt = rs2 & 0b111111;
                            self.set_reg(inst.rd, rs1 << shamt);
                        }
                        (0b0000000, 0b010) => {
                            // SLT
                            if (rs1 as i64) < (rs2 as i64) {
                                self.set_reg(inst.rd, 1);
                            } else {
                                self.set_reg(inst.rd, 0);
                            }
                        }
                        (0b0000000, 0b011) => {
                            // SLTU
                            if (rs1 as u64) < (rs2 as u64) {
                                self.set_reg(inst.rd, 1);
                            } else {
                                self.set_reg(inst.rd, 0);
                            }
                        }
                        (0b0000000, 0b100) => {
                            // XOR
                            self.set_reg(inst.rd, rs1 ^ rs2);
                        }
                        (0b0000000, 0b101) => {
                            // SRL
                            let shamt = rs2 & 0b111111;
                            self.set_reg(inst.rd, rs1 >> shamt);
                        }
                        (0b0100000, 0b101) => {
                            // SRA
                            let shamt = rs2 & 0b111111;
                            self.set_reg(inst.rd,
                                ((rs1 as i64) >> shamt) as u64);
                        }
                        (0b0000000, 0b110) => {
                            // OR
                            self.set_reg(inst.rd, rs1 | rs2);
                        }
                        (0b0000000, 0b111) => {
                            // AND
                            self.set_reg(inst.rd, rs1 & rs2);
                        }
                        _ => unreachable!(),
                    }
                }
                0b0111011 => {
                    // We know it's an Rtype
                    let inst = Rtype::from(inst);

                    let rs1 = self.reg(inst.rs1) as u32;
                    let rs2 = self.reg(inst.rs2) as u32;

                    match (inst.funct7, inst.funct3) {
                        (0b0000000, 0b000) => {
                            // ADDW
                            self.set_reg(inst.rd,
                                rs1.wrapping_add(rs2) as i32 as i64 as u64);
                        }
                        (0b0100000, 0b000) => {
                            // SUBW
                            self.set_reg(inst.rd,
                                rs1.wrapping_sub(rs2) as i32 as i64 as u64);
                        }
                        (0b0000000, 0b001) => {
                            // SLLW
                            let shamt = rs2 & 0b11111;
                            self.set_reg(inst.rd,
                                (rs1 << shamt) as i32 as i64 as u64);
                        }
                        (0b0000000, 0b101) => {
                            // SRLW
                            let shamt = rs2 & 0b11111;
                            self.set_reg(inst.rd,
                                (rs1 >> shamt) as i32 as i64 as u64);
                        }
                        (0b0100000, 0b101) => {
                            // SRAW
                            let shamt = rs2 & 0b11111;
                            self.set_reg(inst.rd,
                                ((rs1 as i32) >> shamt) as i64 as u64);
                        }
                        _ => unreachable!(),
                    }
                }
                0b0001111 => {
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // FENCE
                        }
                        _ => unreachable!(),
                    }
                }
                0b1110011 => {
                    if inst == 0b00000000000000000000000001110011 {
                        // ECALL
                        return Err(VmExit::Syscall);
                    } else if inst == 0b00000000000100000000000001110011 {
                        // EBREAK
                        panic!("EBREAK");
                    } else {
                        unreachable!();
                    }
                }
                0b0011011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);
                    
                    let rs1 = self.reg(inst.rs1) as u32;
                    let imm = inst.imm as u32;

                    match inst.funct3 {
                        0b000 => {
                            // ADDIW
                            self.set_reg(inst.rd,
                                rs1.wrapping_add(imm) as i32 as i64 as u64);
                        }
                        0b001 => {
                            let mode = (inst.imm >> 5) & 0b1111111;
                            
                            match mode {
                                0b0000000 => {
                                    // SLLIW
                                    let shamt = inst.imm & 0b11111;
                                    self.set_reg(inst.rd,
                                        (rs1 << shamt) as i32 as i64 as u64);
                                }
                                _ => unreachable!(),
                            }
                        }
                        0b101 => {
                            let mode = (inst.imm >> 5) & 0b1111111;
                            
                            match mode {
                                0b0000000 => {
                                    // SRLIW
                                    let shamt = inst.imm & 0b11111;
                                    self.set_reg(inst.rd,
                                        (rs1 >> shamt) as i32 as i64 as u64)
                                }
                                0b0100000 => {
                                    // SRAIW
                                    let shamt = inst.imm & 0b11111;
                                    self.set_reg(inst.rd,
                                        ((rs1 as i32) >> shamt) as i64 as u64);
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                _ => unimplemented!("Unhandled opcode {:#09b}\n", opcode),
            }

            // Update PC to the next instruction
            self.set_reg(Register::Pc, pc.wrapping_add(4));
        }
    }
    
    /// Run the VM using the JIT
    pub fn run_jit(&mut self, instrs_execed: &mut u64, 
                   vm_cycles: &mut u64, corpus: &Corpus)
            -> Result<(), VmExit> {
        // Get the JIT addresses
        let (memory, perms, dirty, dirty_bitmap) = self.memory.jit_addrs();

        // If `Some`, we re-entry the JIT by jumping directly to this address,
        // ignoring PC
        let mut override_jit_addr = None;

        loop {
            let mut jit_addr = if let Some(override_jit_addr) =
                    override_jit_addr.take() {
                override_jit_addr
            } else {
                // Get the current PC
                let pc = self.reg(Register::Pc);
                let jit_addr = {
                    let jit_cache = self.jit_cache.as_ref().unwrap();
                    jit_cache.lookup(VirtAddr(pc as usize))
                };

                if let Some(jit_addr) = jit_addr {
                    jit_addr
                } else {
                    // Generate the JIT for this PC
                    let tmp = self.compile_jit(VirtAddr(pc as usize), corpus)?;

                    // Update the JIT tables
                    self.jit_cache.as_ref().unwrap().add_mapping(
                        VirtAddr(pc as usize), &tmp)
                }
            };

            // Set up the JIT state
            self.state.instrs_execed = *instrs_execed;
            self.state.memory        = memory;
            self.state.permissions   = perms;
            self.state.dirty         = dirty;
            self.state.dirty_idx     = self.memory.dirty_len();
            self.state.dirty_bitmap  = dirty_bitmap;
            self.state.trace_buffer  = self.trace.as_ptr() as usize;
            self.state.trace_idx     = self.trace.len();
            self.state.trace_len     = self.trace.capacity();
            self.state.cov_bitmap    =
                corpus.coverage_bitmap.as_ptr() as usize;
                    
            let jit_cache = self.jit_cache.as_ref().unwrap();

            let it = rdtsc();
            'quick_reenter: loop {
                unsafe {
                    // Create a function pointer to the JIT
                    let func =
                        *(&jit_addr as *const usize as
                          *const fn(&mut GuestState));
                    func(&mut self.state);

                    // Quickly check if this is an indirect branch
                    if self.state.exit_reason == ExitReason::IndirectBranch {
                        // Check if we already know the JIT address of the
                        // branch target
                        if let Some(ent) =
                                jit_cache.lookup(
                                    VirtAddr(self.state.reenter_pc as usize)) {
                            jit_addr = ent;
                            continue 'quick_reenter;
                        }
                    }

                    // Either it was not an indirect branch, or we need to lift
                    // the target
                    break 'quick_reenter;
                }
            }
            *vm_cycles += rdtsc() - it;

            // Update instructions executed from JIT state
            *instrs_execed = self.state.instrs_execed;

            // Update the PC reentry point
            self.set_reg(Register::Pc, self.state.reenter_pc);
                    
            unsafe {
                // Update trace length
                self.trace.set_len(self.state.trace_idx);
            
                // Update the dirty state
                self.memory.set_dirty_len(self.state.dirty_idx);
            }

            match self.state.exit_reason {
                ExitReason::None => unreachable!(),
                ExitReason::Coverage => {
                    // Update code coverage
                    let key = (
                        VirtAddr(self.state.cov_from as usize),
                        VirtAddr(self.state.cov_to as usize),
                    );
                    corpus.code_coverage.entry_or_insert(
                        &key, self.state.cov_to as usize, || {
                            // Save the input and log it in the hash table
                            let hash = corpus.hasher.hash(&self.fuzz_input);
                            corpus.input_hashes.entry_or_insert(
                                    &hash, hash as usize, || {
                                corpus.inputs.push(
                                    Box::new(self.fuzz_input.clone()));
                                Box::new(())
                            });

                            Box::new(())
                        });

                    // Fall through to re-execute instruction
                }
                ExitReason::IndirectBranch => {
                    // Just fall through to translate to JIT
                }
                ExitReason::Ebreak => {
                    // RISC-V breakpoint instruction
                    return Err(VmExit::Ebreak);
                }
                ExitReason::Ecall => {
                    // Syscall
                    return Err(VmExit::Syscall);
                }
                ExitReason::ReadFault => {
                    // Read fault
                    // The JIT reports the address of the base of the
                    // access, invoke the emulator to get the specific
                    // byte which caused the fault
                    return self.run_emu(instrs_execed, corpus);
                }
                ExitReason::WriteFault => {
                    // Write fault
                    // The JIT reports the address of the base of the
                    // access, invoke the emulator to get the specific
                    // byte which caused the fault
                    return self.run_emu(instrs_execed, corpus);
                }
                ExitReason::Timeout => {
                    // Hit the instruction count timeout
                    return Err(VmExit::Timeout);
                }
                ExitReason::Breakpoint => {
                    // Hit breakpoint, invoke callback
                    let pc = VirtAddr(self.state.reenter_pc as usize);
                    if let Some(callback) = self.breakpoints.get(&pc) {
                        callback(self)?;
                    }

                    if self.reg(Register::Pc) == self.state.reenter_pc {
                        // Force execution at the return location, which
                        // will skip over the breakpoint return
                        panic!("WAT");
                    } else {
                        // PC was changed by the breakpoint handler,
                        // thus we respect its change and will jump
                        // to the target it specified
                    }
                }
                ExitReason::InvalidOpcode => {
                    // An invalid opcode was executed
                    return Err(VmExit::InvalidOpcode);
                }
            }
        }
    }

    /// Compile a JIT function for `pc` until all paths lead to indirect
    /// jumps or calls
    pub fn compile_jit(&mut self, pc: VirtAddr, corpus: &Corpus)
            -> Result<Vec<u8>, VmExit> {
        let mut visited = BTreeSet::new();
        let mut queued = VecDeque::new();
        
        // Insert the program counter into the queue
        queued.push_back(pc);

        let mut program = String::new();
        program += 
r#"
#include <stddef.h>
#include <stdint.h>

enum _vmexit {
    None,
    IndirectBranch,
    ReadFault,
    WriteFault,
    Ecall,
    Ebreak,
    Timeout,
    Breakpoint,
    InvalidOpcode,
    Coverage,
};

struct _state {
    enum _vmexit exit_reason;
    uint64_t     reenter_pc;

    uint64_t cov_from;
    uint64_t cov_to;

    uint64_t regs[33];
    uint8_t *__restrict const memory;
    uint8_t *__restrict const permissions;
    uintptr_t *__restrict const dirty;
    size_t dirty_idx;
    uint64_t *__restrict const dirty_bitmap;

    uint64_t *__restrict const trace_buffer;
    size_t trace_idx;
    const size_t trace_len;
    uint64_t *const cov_bitmap;
    uint64_t instrs_execed;
    const uint64_t timeout;
};

extern "C" void start(struct _state *__restrict state) {
"#;

        macro_rules! set_reg {
            ($reg:expr, $expr:expr) => {
                if $reg != Register::Zero {
                    program += &format!("    state->regs[{}] = {};\n",
                        $reg as usize, $expr);
                }
            }
        }
        
        macro_rules! get_reg {
            ($expr:expr, $reg:expr) => {
                if $reg == Register::Zero {
                    program += &format!("    {} = 0x0ULL;\n", $expr);
                } else {
                    program += &format!("    {} = state->regs[{}];\n",
                        $expr, $reg as usize);
                }
            }
        }
        
        macro_rules! set_regw {
            ($reg:expr, $expr:expr) => {
                if $reg != Register::Zero {
                    program +=
                        &format!("    state->regs[{}] = (int32_t)({});\n",
                        $reg as usize, $expr);
                }
            }
        }
        
        macro_rules! get_regw {
            ($expr:expr, $reg:expr) => {
                if $reg == Register::Zero {
                    program += &format!("    {} = 0x0U;\n", $expr);
                } else {
                    program +=
                        &format!("    {} = (uint32_t)state->regs[{}];\n",
                        $expr, $reg as usize);
                }
            }
        }

        while let Some(pc) = queued.pop_front() {
            // Attempt to notify of a coverage edge ($from, $to)
            // Note: This will cause the current instruction to be re-executed
            // if the coverage is new. Thus, it is critical that no side
            // effects occur prior to the coverage_event!() macro use.
            macro_rules! coverage_event {
                ($from:expr, $to:expr) => {
                    let coverage_bitmap_bits =
                        size_of_val(corpus.coverage_bitmap.as_slice()) * 8;
                    assert!(coverage_bitmap_bits.count_ones() == 1,
                        "Coverage bitmap must be a power of two");
                    program += &format!(r#"
        if (state->instrs_execed > state->timeout) {{
            state->exit_reason = Timeout;
            state->reenter_pc  = {pc:#x}ULL;
            return;
        }}

        auto hash = ({from} ^ 0xe66dd519dba260bbULL) ^
            ({to} ^ 0xa50ec1c4a4065d15ULL);
        hash ^= hash << 13;
        hash ^= hash >> 17;
        hash ^= hash << 43;
        hash &= {hashmask};
        auto idx = hash / 64;
        auto bit = 1ULL << (hash % 64);
        if ((state->cov_bitmap[idx] & bit) == 0) {{
            state->cov_bitmap[idx] |= bit;
            state->exit_reason = Coverage;
            state->cov_from    = {from};
            state->cov_to      = {to};
            state->reenter_pc  = {pc:#x}ULL;
            return;
        }}
    "#, from = $from, to = $to, hashmask = coverage_bitmap_bits - 1,
        pc = pc.0);
                }
            }

            if !visited.insert(pc) {
                // Already JITted this PC
                continue;
            }

            // Check alignment
            if pc.0 & 3 != 0 {
                // Code was unaligned, return a code fetch fault
                return Err(VmExit::ExecFault(pc));
            }

            // Read the instruction
            let inst: u32 = self.memory.read_perms(pc, Perm(PERM_EXEC))
                .map_err(|x| VmExit::ExecFault(x.is_crash().unwrap().1))?;

            // Create the instruction start label
            program += &format!("inst_{:016x}: {{\n", pc.0);

            // Update instructions executed stats
            program += "    state->instrs_execed += 1;\n";
            
            if ENABLE_TRACING {
                program += &format!(r#"
    if (state->trace_idx >= state->trace_len) {{
        __builtin_trap();
    }}
    for(int ii = 0; ii < 32; ii++) {{
        state->trace_buffer[state->trace_idx * 33 + ii] = state->regs[ii];
    }}
    state->trace_buffer[state->trace_idx * 33 + 32] = {:#x}ULL;
    state->trace_idx++;
"#, pc.0);
            }
            
            // Insert breakpoint if needed
            if self.breakpoints.contains_key(&pc) {
                program += &format!(r#"
    state->exit_reason = Breakpoint;
    state->reenter_pc  = {:#x}ULL;
    return;
"#, pc.0);
            }

            // Extract the opcode from the instruction
            let opcode = inst & 0b1111111;

            match opcode {
                0b0110111 => {
                    // LUI
                    let inst = Utype::from(inst);
                    set_reg!(inst.rd,
                             format!("{:#x}ULL", inst.imm as i64 as u64));
                }
                0b0010111 => {
                    // AUIPC
                    let inst = Utype::from(inst);
                    let val =
                        (inst.imm as i64 as u64).wrapping_add(pc.0 as u64);
                    set_reg!(inst.rd, format!("{:#x}ULL", val));
                }
                0b1101111 => {
                    // JAL
                    let inst = Jtype::from(inst);
                    let retaddr = pc.0.wrapping_add(4);
                    let target  = pc.0.wrapping_add(inst.imm as i64 as usize);

                    // Record coverage
                    coverage_event!(
                        format!("{:#x}ULL", pc.0),
                        format!("{:#x}ULL", target));

                    // Set the return address
                    set_reg!(inst.rd, retaddr);

                    if inst.rd == Register::Zero {
                        // Unconditional branch == jal with an rd = zero
                        program += &format!("goto inst_{:016x};\n", target);
                        queued.push_back(VirtAddr(target));
                    } else {
                        // Function call, treat as an indirect branch to
                        // avoid inlining boatloads of function calls into
                        // their parents.
                        program +=
                            "    state->exit_reason = IndirectBranch;\n";
                        program +=
                            &format!("    state->reenter_pc = {:#x}ULL;\n",
                                target);
                        program += "    return;\n";
                    }

                    program += "}\n";
                    continue;
                }
                0b1100111 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // JALR
                            let retaddr = pc.0.wrapping_add(4);
                            get_reg!("auto target", inst.rs1);
                            program += &format!("    target += {:#x}ULL;\n",
                                inst.imm as i64 as u64);

                            // Record coverage
                            coverage_event!(
                                format!("{:#x}ULL", pc.0),
                                "target");

                            // Set the return address
                            set_reg!(inst.rd, retaddr);

                            program +=
                                "    state->exit_reason = IndirectBranch;\n";
                            program +=
                                "    state->reenter_pc = target;\n";
                            program += "    return;\n";
                            program += "}\n";
                            continue;
                        }
                        _ => unimplemented!("Unexpected 0b1100111"),
                    }
                }
                0b1100011 => {
                    // We know it's an Btype
                    let inst = Btype::from(inst);

                    let (cmptyp, cmpop) = match inst.funct3 {
                        0b000 => /* BEQ  */ ("int64_t",  "=="),
                        0b001 => /* BNE  */ ("int64_t",  "!="),
                        0b100 => /* BLT  */ ("int64_t",  "<"),
                        0b101 => /* BGE  */ ("int64_t",  ">="),
                        0b110 => /* BLTU */ ("uint64_t", "<"),
                        0b111 => /* BGEU */ ("uint64_t", ">="),
                        _ => unimplemented!("Unexpected 0b1100011"),
                    };

                    // Compute branch target
                    let target = pc.0.wrapping_add(inst.imm as i64 as usize);

                    get_reg!("auto rs1", inst.rs1);
                    get_reg!("auto rs2", inst.rs2);
                    program += &format!("    if (({})rs1 {} ({})rs2) {{\n",
                        cmptyp, cmpop, cmptyp);

                    // Record coverage for true condition
                    coverage_event!(
                        format!("{:#x}ULL", pc.0),
                        format!("{:#x}ULL", target));

                    program +=
                        &format!("        goto inst_{:016x};\n", target);
                    program += "    }\n";
                    
                    // Record coverage for false condition
                    coverage_event!(
                        format!("{:#x}ULL", pc.0),
                        format!("{:#x}ULL", pc.0.wrapping_add(4)));

                    // Queue exploration of this target
                    queued.push_back(VirtAddr(target));
                }
                0b0000011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);
                     
                    let (loadtyp, access_size) = match inst.funct3 {
                        0b000 => /* LB  */ ("int8_t",   1),
                        0b001 => /* LH  */ ("int16_t",  2),
                        0b010 => /* LW  */ ("int32_t",  4),
                        0b011 => /* LD  */ ("int64_t",  8),
                        0b100 => /* LBU */ ("uint8_t",  1),
                        0b101 => /* LHU */ ("uint16_t", 2),
                        0b110 => /* LWU */ ("uint32_t", 4),
                        _ => unreachable!(),
                    };
                    
                    // Compute the read permission mask
                    let mut perm_mask = 0u64;
                    for ii in 0..access_size {
                        perm_mask |= (PERM_READ as u64) << (ii * 8)
                    }

                    // Compute the address
                    get_reg!("auto addr", inst.rs1);
                    program += &format!("    addr += {:#x}ULL;\n",
                        inst.imm as i64 as u64);

                    // Check the bounds and permissions of the address
                    program += &format!(r#"
    if(addr > {}ULL - sizeof({}) ||
            (*({}*)(state->permissions + addr) & {:#x}ULL) != {:#x}ULL) {{
        state->exit_reason = ReadFault;
        state->reenter_pc  = {:#x}ULL;
        return;
    }}
    "#, self.memory.len(), loadtyp, loadtyp, perm_mask, perm_mask, pc.0);

                    set_reg!(inst.rd, format!("*({}*)(state->memory + addr)",
                        loadtyp));
                }
                0b0100011 => {
                    // We know it's an Stype
                    let inst = Stype::from(inst);

                    let (storetyp, access_size) =
                            match inst.funct3 {
                        0b000 => /* SB */ ("uint8_t",  1),
                        0b001 => /* SH */ ("uint16_t", 2),
                        0b010 => /* SW */ ("uint32_t", 4),
                        0b011 => /* SD */ ("uint64_t", 8),
                        _ => unreachable!(),
                    };
                    
                    // Compute the write permission mask and the RAW permission
                    // mask
                    let mut perm_mask = 0u64;
                    let mut raw_mask = 0u64;
                    for ii in 0..access_size {
                        perm_mask |= (PERM_WRITE as u64) << (ii * 8);
                        raw_mask  |= (PERM_RAW as u64) << (ii * 8);
                    }
                    
                    // Compute the address
                    get_reg!("auto addr", inst.rs1);
                    program += &format!("    addr += {:#x}ULL;\n",
                        inst.imm as i64 as u64);
                    
                    // Check the bounds and permissions of the address
                    program += &format!(r#"
    if(addr > {}ULL - sizeof({}) ||
            (*({}*)(state->permissions + addr) & {:#x}ULL) != {:#x}ULL) {{
        state->exit_reason = WriteFault;
        state->reenter_pc  = {:#x}ULL;
        return;
    }}

    // Enable reads for memory with RAW set
    auto perms = *({}*)(state->permissions + addr);
    perms &= {:#x}ULL;
    *({}*)(state->permissions + addr) |= perms >> 3;

    auto block = addr / {};
    auto idx   = block / 64;
    auto bit   = 1ULL << (block % 64);
    if((state->dirty_bitmap[idx] & bit) == 0) {{
        state->dirty[state->dirty_idx++] = block;
        state->dirty_bitmap[idx] |= bit;
    }}
    "#, self.memory.len(),
        storetyp, storetyp, perm_mask, perm_mask, pc.0, storetyp, raw_mask,
        storetyp, DIRTY_BLOCK_SIZE);

                    // Write the memory!
                    get_reg!(format!("*({}*)(state->memory + addr)",
                        storetyp), inst.rs2);
                }
                0b0010011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);
                    
                    match inst.funct3 {
                        0b000 => {
                            // ADDI
                            get_reg!("auto rs1", inst.rs1);
                            set_reg!(inst.rd, format!("rs1 + {:#x}ULL",
                                inst.imm as i64 as u64));
                        }
                        0b010 => {
                            // SLTI
                            get_reg!("auto rs1", inst.rs1);
                            set_reg!(inst.rd,
                                format!("((int64_t)rs1 < {:#x}LL) ? 1 : 0",
                                inst.imm as i64));
                        }
                        0b011 => {
                            // SLTIU
                            get_reg!("auto rs1", inst.rs1);
                            set_reg!(inst.rd,
                                format!("((uint64_t)rs1 < {:#x}ULL) ? 1 : 0",
                                inst.imm as i64 as u64));
                        }
                        0b100 => {
                            // XORI
                            get_reg!("auto rs1", inst.rs1);
                            set_reg!(inst.rd, format!("rs1 ^ {:#x}ULL",
                                inst.imm as i64 as u64));
                        }
                        0b110 => {
                            // ORI
                            get_reg!("auto rs1", inst.rs1);
                            set_reg!(inst.rd, format!("rs1 | {:#x}ULL",
                                inst.imm as i64 as u64));
                        }
                        0b111 => {
                            // ANDI
                            get_reg!("auto rs1", inst.rs1);
                            set_reg!(inst.rd, format!("rs1 & {:#x}ULL",
                                inst.imm as i64 as u64));
                        }
                        0b001 => {
                            let mode = (inst.imm >> 6) & 0b111111;
                            
                            match mode {
                                0b000000 => {
                                    // SLLI
                                    let shamt = inst.imm & 0b111111;
                                    get_reg!("auto rs1", inst.rs1);
                                    set_reg!(inst.rd, format!("rs1 << {}",
                                        shamt));
                                }
                                _ => unreachable!(),
                            }
                        }
                        0b101 => {
                            let mode = (inst.imm >> 6) & 0b111111;
                            
                            match mode {
                                0b000000 => {
                                    // SRLI
                                    let shamt = inst.imm & 0b111111;
                                    get_reg!("auto rs1", inst.rs1);
                                    set_reg!(inst.rd, format!("rs1 >> {}",
                                        shamt));
                                }
                                0b010000 => {
                                    // SRAI
                                    let shamt = inst.imm & 0b111111;
                                    get_reg!("auto rs1", inst.rs1);
                                    set_reg!(inst.rd,
                                             format!("(int64_t)rs1 >> {}",
                                        shamt));
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                0b0110011 => {
                    // We know it's an Rtype
                    let inst = Rtype::from(inst);

                    match (inst.funct7, inst.funct3) {
                        (0b0000000, 0b000) => {
                            // ADD
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd, "rs1 + rs2");
                        }
                        (0b0100000, 0b000) => {
                            // SUB
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd, "rs1 - rs2");
                        }
                        (0b0000000, 0b001) => {
                            // SLL
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd, "rs1 << (rs2 & 0x3f)");
                        }
                        (0b0000000, 0b010) => {
                            // SLT
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd,
                                "((int64_t)rs1 < (int64_t)rs2) ? 1 : 0");
                        }
                        (0b0000000, 0b011) => {
                            // SLTU
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd,
                                "((uint64_t)rs1 < (uint64_t)rs2) ? 1 : 0");
                        }
                        (0b0000000, 0b100) => {
                            // XOR
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd, "rs1 ^ rs2");
                        }
                        (0b0000000, 0b101) => {
                            // SRL
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd, "rs1 >> (rs2 & 0x3f)");
                        }
                        (0b0100000, 0b101) => {
                            // SRA
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd,
                                     "(int64_t)rs1 >> ((int64_t)rs2 & 0x3f)");
                        }
                        (0b0000000, 0b110) => {
                            // OR
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd, "rs1 | rs2");
                        }
                        (0b0000000, 0b111) => {
                            // AND
                            get_reg!("auto rs1", inst.rs1);
                            get_reg!("auto rs2", inst.rs2);
                            set_reg!(inst.rd, "rs1 & rs2");
                        }
                        _ => unreachable!(),
                    }
                }
                0b0111011 => {
                    // We know it's an Rtype
                    let inst = Rtype::from(inst);

                    match (inst.funct7, inst.funct3) {
                        (0b0000000, 0b000) => {
                            // ADDW
                            get_regw!("auto rs1", inst.rs1);
                            get_regw!("auto rs2", inst.rs2);
                            set_regw!(inst.rd, "rs1 + rs2");
                        }
                        (0b0100000, 0b000) => {
                            // SUBW
                            get_regw!("auto rs1", inst.rs1);
                            get_regw!("auto rs2", inst.rs2);
                            set_regw!(inst.rd, "rs1 - rs2");
                        }
                        (0b0000000, 0b001) => {
                            // SLLW
                            get_regw!("auto rs1", inst.rs1);
                            get_regw!("auto rs2", inst.rs2);
                            set_regw!(inst.rd, "rs1 << (rs2 & 0x1f)");
                        }
                        (0b0000000, 0b101) => {
                            // SRLW
                            get_regw!("auto rs1", inst.rs1);
                            get_regw!("auto rs2", inst.rs2);
                            set_regw!(inst.rd, "rs1 >> (rs2 & 0x1f)");
                        }
                        (0b0100000, 0b101) => {
                            // SRAW
                            get_regw!("auto rs1", inst.rs1);
                            get_regw!("auto rs2", inst.rs2);
                            set_regw!(inst.rd,
                                     "(int32_t)rs1 >> ((int32_t)rs2 & 0x1f)");
                        }
                        _ => unreachable!(),
                    }
                }
                0b0001111 => {
                    let inst = Itype::from(inst);

                    match inst.funct3 {
                        0b000 => {
                            // FENCE
                        }
                        _ => unreachable!(),
                    }
                }
                0b1110011 => {
                    if inst == 0b00000000000000000000000001110011 {
                        // ECALL
                        program += &format!(r#"
    state->exit_reason = Ecall;
    state->reenter_pc  = {:#x}ULL;
    return;
"#, pc.0);
                    } else if inst == 0b00000000000100000000000001110011 {
                        // EBREAK
                        program += &format!(r#"
    state->exit_reason = Ebreak;
    state->reenter_pc  = {:#x}ULL;
    return;
"#, pc.0);
                    } else {
                        unreachable!();
                    }
                }
                0b0011011 => {
                    // We know it's an Itype
                    let inst = Itype::from(inst);
                    
                    match inst.funct3 {
                        0b000 => {
                            // ADDIW
                            get_regw!("auto rs1", inst.rs1);
                            set_regw!(inst.rd, format!("rs1 + {}U",
                                inst.imm as i32 as u32));
                        }
                        0b001 => {
                            let mode = (inst.imm >> 5) & 0b1111111;
                            
                            match mode {
                                0b0000000 => {
                                    // SLLIW
                                    let shamt = inst.imm & 0b11111;
                                    get_regw!("auto rs1", inst.rs1);
                                    set_regw!(inst.rd,
                                        format!("rs1 << {}",
                                        shamt));
                                }
                                _ => unreachable!(),
                            }
                        }
                        0b101 => {
                            let mode = (inst.imm >> 5) & 0b1111111;
                            
                            match mode {
                                0b0000000 => {
                                    // SRLIW
                                    let shamt = inst.imm & 0b11111;
                                    get_regw!("auto rs1", inst.rs1);
                                    set_regw!(inst.rd,
                                        format!("rs1 >> {}",
                                        shamt));
                                }
                                0b0100000 => {
                                    // SRAIW
                                    let shamt = inst.imm & 0b11111;
                                    get_regw!("auto rs1", inst.rs1);
                                    set_regw!(inst.rd,
                                        format!("(int32_t)rs1 >> {}",
                                        shamt));
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                _ => unimplemented!("Unhandled opcode {:#09b}\n", opcode),
            }

            let next_inst = pc.0.wrapping_add(4);
            program += "}\n";
            program += &format!("    goto inst_{:016x};\n", next_inst);
            queued.push_back(VirtAddr(next_inst));
        }

        // Close the function scope
        program += "}\n";
        
        // Hash the C++ file contents
        let proghash = corpus.hasher.hash(program.as_bytes());

        // Check if we're the first core to try to compile this
        let first = {
            let mut jobs = corpus.compile_jobs.lock().unwrap();
            jobs.insert(proghash)
        };
        
        // Create the jitcache folder
        std::fs::create_dir_all("jitcache")
            .expect("Failed to create jitcache directory");

        // Create the cache name
        let cachename = Path::new("jitcache")
            .join(format!("{:032x}", proghash));

        // If we aren't the first to access the cache, idle loop until the
        // first person has compiled the code
        if !first {
            while !cachename.exists() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        // If the cache exists, read the cache
        if cachename.exists() {
            return Ok(std::fs::read(&cachename)
                .expect("Failed to read file from jit cache"));
        }
        
        print!("Compiling cache for {:#018x} -> {:032x}\n", pc.0, proghash);

        let cppfn = std::env::temp_dir().join(
            format!("fwetmp_{:?}.cpp",
                    std::thread::current().id()));
        let linkfn = std::env::temp_dir().join(
            format!("fwetmp_{:?}.lunk",
                    std::thread::current().id()));
        let binfn = std::env::temp_dir().join(
            format!("fwetmp_{:?}.bin",
                    std::thread::current().id()));

        // Write out the test program
        std::fs::write(&cppfn, program)
            .expect("Failed to write program");

        // Create the ELF
        let res = Command::new("clang++").args(&[
            "-O3", "-march=native", "-Wall",
            "-fno-asynchronous-unwind-tables",
            "-Wno-unused-label",
            "-Wno-unused-variable",
            "-Werror",
            "-fno-strict-aliasing",
            "-static", "-nostdlib", "-ffreestanding",
            "-Wl,-Tldscript.ld", "-Wl,--gc-sections", "-Wl,--build-id=none",
            "-o", linkfn.to_str().unwrap(),
            cppfn.to_str().unwrap()]).status()
            .expect("Failed to launch clang++");
        assert!(res.success(), "clang++ returned error");

        // Convert the ELF to a binary
        let res = Command::new("objcopy")
            .args(&["-O", "binary", "--remove-section=.note.gnu.property",
                    linkfn.to_str().unwrap(),
                    binfn.to_str().unwrap()]).status()
            .expect("Failed to launch objcopy");
        assert!(res.success(), "objcopy returned error");

        // Move the compiled output to the cache
        std::fs::rename(&binfn, &cachename)
            .expect("Failed to rename compiled JIT to cache file");

        Ok(std::fs::read(&cachename).expect("Failed to read JIT code"))
    }
}

