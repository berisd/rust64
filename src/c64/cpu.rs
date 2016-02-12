// The CPU
#![allow(non_snake_case)]
//extern crate sdl2;
use c64::opcodes;
use c64::memory;
use c64::vic;
use c64::cia;
use std::cell::RefCell;
use std::rc::Rc;

use utils;

pub type CPUShared = Rc<RefCell<CPU>>;


// status flags for P register
pub enum StatusFlag
{
    Carry            = 1 << 0,
    Zero             = 1 << 1,
    InterruptDisable = 1 << 2,
    DecimalMode      = 1 << 3,
    Break            = 1 << 4,
    Unused           = 1 << 5,
    Overflow         = 1 << 6,
    Negative         = 1 << 7,
}

// action to perform on specific CIA and VIC events
pub enum CallbackAction
{
    None,
    TriggerVICIrq,
    ClearVICIrq,
    TriggerCIAIrq,
    ClearCIAIrq,
    TriggerNMI,
    ClearNMI
}

pub static NMI_VECTOR:   u16 = 0xFFFA;
pub static RESET_VECTOR: u16 = 0xFFFC;
pub static IRQ_VECTOR:   u16 = 0xFFFE;

pub enum CPUState
{
    FetchOp,
    FetchOperandAddr,
    PerformRMW,
    ProcessIRQ,
    ProcessNMI,
    ExecuteOp
}

pub struct CPU
{
    pub PC: u16, // program counter
    pub SP: u8,  // stack pointer
    pub P: u8,   // processor status
    pub A: u8,   // accumulator
    pub X: u8,   // index register
    pub Y: u8,   // index register
    pub mem_ref: Option<memory::MemShared>, // reference to shared system memory
    pub vic_ref: Option<vic::VICShared>,
    pub cia1_ref: Option<cia::CIAShared>,
    pub cia2_ref: Option<cia::CIAShared>,
    pub instruction: opcodes::Instruction,
    pub ba_low: bool,  // is BA low?
    pub cia_irq: bool,
    pub vic_irq: bool,
    pub irq_cycles_left: u8,
    pub nmi_cycles_left: u8,
    pub first_nmi_cycle: u32,
    pub first_irq_cycle: u32,
    pub state: CPUState,
    pub nmi: bool,
    pub debug_instr: bool,
    pub prev_PC: u16, // previous program counter - for debugging
    dfff_byte: u8,
    pub op_debugger: utils::OpDebugger
}

impl CPU
{
    pub fn new_shared() -> CPUShared
    {
        Rc::new(RefCell::new(CPU
        {
            PC: 0,
            SP: 0xFF,
            P: 0,
            A: 0,
            X: 0,
            Y: 0,
            mem_ref: None,
            vic_ref: None,
            cia1_ref: None,
            cia2_ref: None,
            ba_low: false,
            cia_irq: false,
            vic_irq: false,
            irq_cycles_left: 0,
            nmi_cycles_left: 0,
            first_nmi_cycle: 0,
            first_irq_cycle: 0,
            state: CPUState::FetchOp,
            instruction: opcodes::Instruction::new(),
            nmi: false,
            debug_instr: false,
            prev_PC: 0,
            dfff_byte: 0x55,
            op_debugger: utils::OpDebugger::new()
        }))
    }

    pub fn set_references(&mut self, memref: memory::MemShared, vicref: vic::VICShared, cia1ref: cia::CIAShared, cia2ref: cia::CIAShared)
    {
        self.mem_ref = Some(memref);
        self.vic_ref = Some(vicref);
        self.cia1_ref = Some(cia1ref);
        self.cia2_ref = Some(cia2ref);
    }    
    
    pub fn set_status_flag(&mut self, flag: StatusFlag, value: bool)
    {
        if value { self.P |=   flag as u8;  }
        else     { self.P &= !(flag as u8); }
    }

    pub fn get_status_flag(&mut self, flag: StatusFlag) -> bool
    {
        self.P & flag as u8 != 0x00
    }

    // these flags will be set in tandem quite often
    pub fn set_zn_flags(&mut self, value: u8)
    {
        self.set_status_flag(StatusFlag::Zero, value == 0x00);
        self.set_status_flag(StatusFlag::Negative, (value as i8) < 0);
    }
    
    pub fn reset(&mut self)
    {
        let pc = self.read_word_le(RESET_VECTOR);
        self.PC = pc;
    }

    pub fn update(&mut self, c64_cycle_cnt: u32)
    {
        // check for irq and nmi
        match self.state
        {
            CPUState::FetchOp => {
                if self.nmi && self.nmi_cycles_left == 0 && (c64_cycle_cnt - (self.first_nmi_cycle as u32) >= 2)
                {
                    self.nmi_cycles_left = 7;
                    self.state = CPUState::ProcessNMI;
                }
                else if (self.cia_irq || self.vic_irq) && self.irq_cycles_left == 0 && !self.get_status_flag(StatusFlag::InterruptDisable) && (c64_cycle_cnt - (self.first_irq_cycle as u32) >= 2)
                {
                    self.irq_cycles_left = 7;
                    self.state = CPUState::ProcessIRQ;
                }
            },
            _ => {}
        }
        
        match self.state
        {
            CPUState::FetchOp => {
                if self.ba_low { return; }
                let next_op = self.next_byte();
                match opcodes::get_instruction(next_op) {
                    Some((opcode, total_cycles, is_rmw, addr_mode)) => {
                        self.instruction.opcode = opcode;
                        self.instruction.addr_mode = addr_mode;
                        self.instruction.is_rmw = is_rmw;
                        self.instruction.calculate_cycles(total_cycles, is_rmw);
                        if self.debug_instr { utils::debug_instruction(next_op, self); }
                    }
                    None => panic!("Can't fetch instruction")
                }

                // jump straight to op execution unless operand address needs to be fetched
                match self.instruction.addr_mode {
                    opcodes::AddrMode::Implied     => self.state = CPUState::ExecuteOp,
                    opcodes::AddrMode::Accumulator => self.state = CPUState::ExecuteOp,
                    opcodes::AddrMode::Immediate   => self.state = CPUState::ExecuteOp,
                    opcodes::AddrMode::Relative    => {
                        // TODO: inc PC only during op execution?
                        let base = (self.PC + 1) as i16;
                        let offset = self.next_byte() as i8;
                        self.instruction.operand_addr = (base + offset as i16) as u16;
                        self.state = CPUState::ExecuteOp;
                    },
                    _ => self.state = CPUState::FetchOperandAddr,
                };
            },
            CPUState::FetchOperandAddr => {
                if self.ba_low { return; }
                if opcodes::fetch_operand_addr(self)
                {
                    if self.instruction.is_rmw
                    {
                        self.state = CPUState::PerformRMW;
                    }
                    else
                    {
                        self.state = CPUState::ExecuteOp;
                    }
                }

                // TODO: odd case? Some instructions can be executed immediately after operand fetch
                if self.instruction.cycles_to_run == 0 && self.instruction.cycles_to_fetch == 0
                {
                    //panic!("Not sure if this should happen - reinvestigate");
                    opcodes::run(self);
                    self.state = CPUState::FetchOp;
                }
            }
            CPUState::ProcessIRQ => {
                if self.process_irq()
                {
                    self.cia_irq = false;
                    self.vic_irq = false;
                    self.state = CPUState::FetchOp;
                }
            },
            CPUState::ProcessNMI => {
                if self.process_nmi()
                {
                    self.nmi = false;
                    self.state = CPUState::FetchOp;
                }
            },
            CPUState::PerformRMW => {
                match self.instruction.cycles_to_rmw
                {
                    2 => {
                        if self.ba_low { return; }
                        let addr = self.instruction.operand_addr;
                        self.instruction.rmw_buffer = self.read_byte(addr);
                    },
                    1 => {
                        let addr = self.instruction.operand_addr;
                        let val = self.instruction.rmw_buffer;
                        self.write_byte(addr, val);
                        self.state = CPUState::ExecuteOp;
                    },
                     _ => panic!("Too many cycles in RMW stage! ({}) ", self.instruction.cycles_to_rmw)
                }

                self.instruction.cycles_to_rmw -= 1;
            },
            CPUState::ExecuteOp => {
                if opcodes::run(self)
                {
                    self.state = CPUState::FetchOp;
                }
            }
        }
    }

    pub fn next_byte(&mut self) -> u8
    {
        let pc = self.PC;
        let op = self.read_byte(pc);
        self.PC += 1;
        op
    }

    pub fn next_word(&mut self) -> u16
    {
        let word = self.read_word_le(self.PC);
        self.PC += 2;
        word
    }

    // stack memory: $0100 - $01FF (256 byes)
    // TODO: some extra message if stack over/underflow occurs? (right now handled by Rust)
    pub fn push_byte(&mut self, value: u8)
    {
        self.SP -= 0x01;
        let newSP = (self.SP + 0x01) as u16;
        self.write_byte(0x0100 + newSP, value);
    }

    pub fn pop_byte(&mut self) -> u8
    {
        let addr = 0x0100 + (self.SP + 0x01) as u16;
        let value = self.read_byte(addr);
        self.SP += 0x01;
        value
    }

    pub fn push_word(&mut self, value: u16)
    {
        self.push_byte(((value >> 8) & 0xFF) as u8);
        self.push_byte((value & 0xFF) as u8);
    }

    pub fn pop_word(&mut self) -> u16
    {
        let lo = (self.pop_byte() as u16) & 0x00FF;
        let hi = (self.pop_byte() as u16) & 0x00FF;
        (hi << 8) | lo
    }

    pub fn write_byte(&mut self, addr: u16, value: u8) -> bool
    {
        let mut write_callback = CallbackAction::None;
        let mut mem_write_ok = true;
        let io_enabled = as_ref!(self.mem_ref).io_on;

        match addr
        {
            // VIC-II address space
            0xD000...0xD3FF => {
                if io_enabled
                {
                    as_mut!(self.vic_ref).write_register(addr, value, &mut write_callback);
                }
                else
                {
                    mem_write_ok = as_mut!(self.mem_ref).write_byte(addr, value);
                }
            },
            // color RAM address space
            0xD800...0xDBFF => {
                if io_enabled
                {
                    mem_write_ok = as_mut!(self.mem_ref).write_byte(addr, value & 0x0F);
                }
                else
                {
                    mem_write_ok = as_mut!(self.mem_ref).write_byte(addr, value);
                }
            },
            // CIA1 address space
            0xDC00...0xDCFF => {
                if io_enabled
                {
                    as_mut!(self.cia1_ref).write_register(addr, value, &mut write_callback);
                }
                else
                {
                    mem_write_ok = as_mut!(self.mem_ref).write_byte(addr, value);
                }
            },
            // CIA2 address space
            0xDD00...0xDDFF => {
                if io_enabled
                {
                    as_mut!(self.cia2_ref).write_register(addr, value, &mut write_callback);
                }
                else
                {
                    mem_write_ok = as_mut!(self.mem_ref).write_byte(addr, value);
                }
            },
            _ => mem_write_ok = as_mut!(self.mem_ref).write_byte(addr, value),
        }

        // on VIC/CIA register write perform necessary action on the CPU
        match write_callback
        {
            CallbackAction::TriggerVICIrq => self.set_vic_irq(true),
            CallbackAction::ClearVICIrq   => self.set_vic_irq(false),
            CallbackAction::TriggerCIAIrq => self.set_cia_irq(true),
            CallbackAction::ClearCIAIrq   => self.set_cia_irq(false),
            CallbackAction::TriggerNMI    => self.set_nmi(true),
            CallbackAction::ClearNMI      => self.set_nmi(false),
            _ => (),
        }

        mem_write_ok
    }

    pub fn read_idle(&mut self, addr: u16)
    {
        let _ = self.read_byte(addr);
    }
    
    pub fn read_byte(&mut self, addr: u16) -> u8
    {
        let byte: u8;
        let mut read_callback = CallbackAction::None;
        let io_enabled = as_ref!(self.mem_ref).io_on;
        match addr
        {
            // VIC-II address space
            0xD000...0xD3FF => {
                if io_enabled
                {
                    byte = as_mut!(self.vic_ref).read_register(addr);
                }
                else
                {
                    byte = as_mut!(self.mem_ref).read_byte(addr);
                }
            },
            // color RAM address space
            0xD800...0xDBFF => {
                if io_enabled
                {
                    byte = (as_ref!(self.mem_ref).read_byte(addr) & 0x0F) | (as_ref!(self.vic_ref).last_byte & 0xF0);
                }
                else
                {
                    byte = as_mut!(self.mem_ref).read_byte(addr);
                }
            },
            // CIA1 address space
            0xDC00...0xDCFF => {
                if io_enabled
                {
                    byte = as_mut!(self.cia1_ref).read_register(addr, &mut read_callback);
                }
                else
                {
                    byte = as_mut!(self.mem_ref).read_byte(addr);
                }
            },
            // CIA2 address space
            0xDD00...0xDDFF => {
                if io_enabled
                {
                    byte = as_mut!(self.cia2_ref).read_register(addr, &mut read_callback);
                }
                else
                {
                    byte = as_mut!(self.mem_ref).read_byte(addr);
                }
            },
            0xDF00...0xDF9F => {
                if io_enabled
                {
                    byte = as_ref!(self.vic_ref).last_byte;
                }
                else
                {
                    byte = as_mut!(self.mem_ref).read_byte(addr);
                }
            },
            0xDFFF => {
                if io_enabled
                {
                    self.dfff_byte = !self.dfff_byte;
                    byte = self.dfff_byte;
                }
                else
                {
                    byte = as_mut!(self.mem_ref).read_byte(addr);
                }
            }, 
            _ => byte = as_mut!(self.mem_ref).read_byte(addr)
        }

        match read_callback
        {
            CallbackAction::TriggerCIAIrq => self.set_cia_irq(true),
            CallbackAction::ClearCIAIrq   => self.set_cia_irq(false),
            CallbackAction::TriggerNMI    => self.set_nmi(true),
            CallbackAction::ClearNMI      => self.set_nmi(false),
            _ => (),
        }

        byte
    }

    pub fn read_word_le(&self, addr: u16) -> u16
    {
        as_ref!(self.mem_ref).read_word_le(addr)
    }

    pub fn write_word_le(&self, addr: u16, value: u16) -> bool
    {
        as_ref!(self.mem_ref).write_word_le(addr, value)
    }
    
    fn process_nmi(&mut self) -> bool
    {
        match self.nmi_cycles_left
        {
            7 => {
                if self.ba_low { return false; }
                let pc = self.PC;
                self.read_idle(pc);
            },
            6 => {
                if self.ba_low { return false; }
                let pc = self.PC;
                self.read_idle(pc);
            },
            5 => {
                let pc_hi = (self.PC >> 8) as u8;
                self.push_byte(pc_hi);
            },
            4 => {
                let pc_lo = self.PC as u8;
                self.push_byte(pc_lo);
            },
            3 => {
                //self.set_status_flag(StatusFlag::Break, false); // TODO: clear brk flag?
                let curr_p = self.P;
                self.push_byte(curr_p);
                self.set_status_flag(StatusFlag::InterruptDisable, true);
            },
            2 => {
                if self.ba_low { return false; } // TODO: is reading whole word ok in cycle 1?
            },
            1 => {
                if self.ba_low { return false; }
                self.PC = as_ref!(self.mem_ref).read_word_le(NMI_VECTOR);
            }
            _ => panic!("Invalid NMI cycle")
        }

        self.nmi_cycles_left -= 1;
        self.nmi_cycles_left == 0
    }
    
    fn process_irq(&mut self) -> bool
    {
        match self.irq_cycles_left
        {
            7 => {
                if self.ba_low { return false; }
                let pc = self.PC;
                self.read_idle(pc);
            },
            6 => {
                if self.ba_low { return false; }
                let pc = self.PC;
                self.read_idle(pc);
            },
            5 => {
                let pc_hi = (self.PC >> 8) as u8;
                self.push_byte(pc_hi);
            },
            4 => {
                let pc_lo = self.PC as u8;
                self.push_byte(pc_lo);
            },
            3 => {
                self.set_status_flag(StatusFlag::Break, false);
                let curr_p = self.P;
                self.push_byte(curr_p);
                self.set_status_flag(StatusFlag::InterruptDisable, true);
            },
            2 => {
                if self.ba_low { return false; } // TODO: is reading whole word ok in cycle 1?
            },
            1 => {
                if self.ba_low { return false; }
                self.PC = as_ref!(self.mem_ref).read_word_le(IRQ_VECTOR);
            }
            _ => panic!("Invalid IRQ cycle")
        }

        self.irq_cycles_left -= 1;
        self.irq_cycles_left == 0
    }

    pub fn set_vic_irq(&mut self, val: bool)
    {
        self.vic_irq = val;
    }

    pub fn set_nmi(&mut self, val: bool)
    {
        self.nmi = val;
    }

    pub fn set_cia_irq(&mut self, val: bool)
    {
        self.cia_irq = val;
    }
    
    pub fn get_operand(&mut self) -> u8
    {
        // RMW instruction store pre-fetched operand value in internal buffer
        if self.instruction.is_rmw
        {
            return self.instruction.rmw_buffer;
        }
        
        let val = match self.instruction.addr_mode {
            opcodes::AddrMode::Implied     => panic!("Can't get operand value!"),
            opcodes::AddrMode::Accumulator => self.A,
            opcodes::AddrMode::Immediate   => self.next_byte(),
            _ => {
                let addr = self.instruction.operand_addr;
                self.read_byte(addr)
            }
        };

        val
    }

    pub fn set_operand(&mut self, val: u8)
    {
        match self.instruction.addr_mode {
            opcodes::AddrMode::Implied     => panic!("Can't set implied operand value!"),
            opcodes::AddrMode::Accumulator => self.A = val,
            opcodes::AddrMode::Immediate   => panic!("Can't set immediate operand value!"),
            opcodes::AddrMode::Relative    => panic!("Can't set relative operand value!"),
            _ => {
                let addr = self.instruction.operand_addr;
                let _ = self.write_byte(addr, val);
            }
        }
    }
}
