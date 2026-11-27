//! A straightforward interpreter over decoded Ops.
//!
//! Values are u64 slots on one operand stack (i32 and f32 use the low 32
//! bits). Control flow uses a label stack; every Block/If already knows where
//! its End is, so branches are O(1) apart from moving result values.

use crate::module::{ExportKind, FuncType, Module, Op, ValType};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    Unreachable,
    DivZero,
    Overflow,
    OutOfBounds,
    BadConversion,
    Indirect,
    StackExhausted,
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Trap::Unreachable => "unreachable",
            Trap::DivZero => "div-zero",
            Trap::Overflow => "overflow",
            Trap::OutOfBounds => "oob",
            Trap::BadConversion => "bad-conversion",
            Trap::Indirect => "indirect",
            Trap::StackExhausted => "stack",
        })
    }
}

pub const PAGE: usize = 65536;
const MAX_DEPTH: usize = 2000;

pub struct Instance<'m> {
    pub m: &'m Module,
    pub memory: Vec<u8>,
    max_pages: u32,
    pub globals: Vec<u64>,
    table: Vec<Option<u32>>,
    stack: Vec<u64>,
    depth: usize,
}

#[derive(Debug)]
pub enum InstantiateError {
    Link(String),
    Trap(Trap),
}

struct Label {
    /// op index to continue at when branching here
    target: usize,
    height: usize,
    arity: usize,
}

impl<'m> Instance<'m> {
    pub fn new(m: &'m Module) -> Result<Instance<'m>, InstantiateError> {
        let (min, max) = m.memory.unwrap_or((0, Some(0)));
        let mut inst = Instance {
            m,
            memory: vec![0; min as usize * PAGE],
            max_pages: max.unwrap_or(65536).min(65536),
            globals: m.globals.iter().map(|g| g.init).collect(),
            table: vec![None; m.table.map_or(0, |t| t.0 as usize)],
            stack: Vec::with_capacity(1024),
            depth: 0,
        };
        for (off, fs) in &m.elems {
            let end = *off as usize + fs.len();
            if end > inst.table.len() {
                return Err(InstantiateError::Link("out of bounds table access".into()));
            }
            for (i, f) in fs.iter().enumerate() {
                inst.table[*off as usize + i] = Some(*f);
            }
        }
        for (off, bytes) in &m.data {
            let end = *off as usize + bytes.len();
            if end > inst.memory.len() {
                return Err(InstantiateError::Link("out of bounds memory access".into()));
            }
            inst.memory[*off as usize..end].copy_from_slice(bytes);
        }
        if let Some(s) = m.start {
            inst.invoke(s, &[]).map_err(InstantiateError::Trap)?;
        }
        Ok(inst)
    }

    pub fn export(&self, name: &str) -> Option<u32> {
        self.m.exports.iter().find(|(n, k, _)| n == name && *k == ExportKind::Func).map(|e| e.2)
    }

    pub fn func_type(&self, f: u32) -> &FuncType {
        &self.m.types[self.m.funcs[f as usize].type_idx as usize]
    }

    pub fn invoke(&mut self, f: u32, args: &[u64]) -> Result<Vec<u64>, Trap> {
        let base = self.stack.len();
        self.stack.extend_from_slice(args);
        let r = self.call(f);
        let out = self.stack.split_off(base);
        r.map(|_| out)
    }

    /// Call function f with its arguments on top of the stack; leaves its results there.
    fn call(&mut self, f: u32) -> Result<(), Trap> {
        if self.depth >= MAX_DEPTH {
            return Err(Trap::StackExhausted);
        }
        self.depth += 1;
        let r = self.run(f);
        self.depth -= 1;
        r
    }

    fn run(&mut self, fi: u32) -> Result<(), Trap> {
        let m = self.m;
        let func = &m.funcs[fi as usize];
        let ty = &m.types[func.type_idx as usize];
        let nparams = ty.params.len();
        let mut locals: Vec<u64> = Vec::with_capacity(nparams + func.locals.len());
        let args_at = self.stack.len().checked_sub(nparams).ok_or(Trap::Unreachable)?;
        locals.extend_from_slice(&self.stack[args_at..]);
        self.stack.truncate(args_at);
        locals.resize(nparams + func.locals.len(), 0);
        let code = &func.code;
        let mut labels: Vec<Label> = vec![Label { target: code.len() - 1, height: self.stack.len(), arity: ty.results.len() }];
        let mut pc = 0usize;
        macro_rules! pop {
            () => {
                self.stack.pop().ok_or(Trap::Unreachable)?
            };
        }
        macro_rules! push {
            ($v:expr) => {
                self.stack.push($v)
            };
        }
        loop {
            let op = code[pc];
            pc += 1;
            match op {
                Op::Unreachable => return Err(Trap::Unreachable),
                Op::Nop => {}
                Op::Block { end, arity } => labels.push(Label { target: end as usize, height: self.stack.len(), arity: arity as usize }),
                Op::Loop => labels.push(Label { target: pc - 1, height: self.stack.len(), arity: 0 }),
                Op::If { else_, end, arity } => {
                    let c = pop!() as u32;
                    labels.push(Label { target: end as usize, height: self.stack.len(), arity: arity as usize });
                    if c == 0 {
                        if else_ != 0 {
                            pc = else_ as usize + 1;
                        } else {
                            pc = end as usize; // the End pops the label
                        }
                    }
                }
                Op::Else { end } => pc = end as usize, // end of the then-arm
                Op::End => {
                    labels.pop();
                    if labels.is_empty() {
                        return Ok(());
                    }
                }
                Op::Br(d) => pc = self.branch(&mut labels, d, code)?,
                Op::BrIf(d) => {
                    if pop!() as u32 != 0 {
                        pc = self.branch(&mut labels, d, code)?;
                    }
                }
                Op::BrTable(t) => {
                    let (ls, def) = &func.br_tables[t as usize];
                    let i = pop!() as u32 as usize;
                    let d = *ls.get(i).unwrap_or(def);
                    pc = self.branch(&mut labels, d, code)?;
                }
                Op::Return => {
                    let d = labels.len() as u32 - 1;
                    pc = self.branch(&mut labels, d, code)?;
                }
                Op::Call(f) => self.call(f)?,
                Op::CallIndirect(t) => {
                    let i = pop!() as u32 as usize;
                    let f = self.table.get(i).copied().flatten().ok_or(Trap::Indirect)?;
                    if m.types[m.funcs[f as usize].type_idx as usize] != m.types[t as usize] {
                        return Err(Trap::Indirect);
                    }
                    self.call(f)?;
                }
                Op::Drop => {
                    pop!();
                }
                Op::Select => {
                    let c = pop!() as u32;
                    let b = pop!();
                    let a = pop!();
                    push!(if c != 0 { a } else { b });
                }
                Op::LocalGet(i) => push!(locals[i as usize]),
                Op::LocalSet(i) => locals[i as usize] = pop!(),
                Op::LocalTee(i) => locals[i as usize] = *self.stack.last().ok_or(Trap::Unreachable)?,
                Op::GlobalGet(i) => push!(self.globals[i as usize]),
                Op::GlobalSet(i) => self.globals[i as usize] = pop!(),
                Op::Load { op, offset } => {
                    let a = pop!() as u32 as u64 + offset as u64;
                    let v = self.load(op, a)?;
                    push!(v);
                }
                Op::Store { op, offset } => {
                    let v = pop!();
                    let a = pop!() as u32 as u64 + offset as u64;
                    self.store(op, a, v)?;
                }
                Op::MemorySize => push!((self.memory.len() / PAGE) as u64),
                Op::MemoryGrow => {
                    let n = pop!() as u32;
                    let old = (self.memory.len() / PAGE) as u32;
                    if old as u64 + n as u64 > self.max_pages as u64 {
                        push!(u32::MAX as u64);
                    } else {
                        self.memory.resize((old + n) as usize * PAGE, 0);
                        push!(old as u64);
                    }
                }
                Op::I32Const(v) => push!(v as u32 as u64),
                Op::I64Const(v) => push!(v as u64),
                Op::F32Const(v) => push!(v as u64),
                Op::F64Const(v) => push!(v),
                Op::Num(n) => self.numeric(n)?,
                Op::TruncSat(n) => {
                    let x = pop!();
                    push!(trunc_sat(n, x));
                }
            }
        }
    }

    fn branch(&mut self, labels: &mut Vec<Label>, d: u32, code: &[Op]) -> Result<usize, Trap> {
        let idx = labels.len().checked_sub(1 + d as usize).ok_or(Trap::Unreachable)?;
        let l = &labels[idx];
        let (target, height, arity) = (l.target, l.height, l.arity);
        let top = self.stack.len().checked_sub(arity).ok_or(Trap::Unreachable)?;
        if top < height {
            return Err(Trap::Unreachable);
        }
        self.stack.drain(height..top);
        if matches!(code[target], Op::Loop) {
            labels.truncate(idx);
            Ok(target) // re-enter the loop, which pushes its label again
        } else {
            labels.truncate(idx + 1);
            Ok(target) // the End at target pops the label
        }
    }

    fn mem(&self, a: u64, n: u64) -> Result<usize, Trap> {
        if a + n > self.memory.len() as u64 {
            Err(Trap::OutOfBounds)
        } else {
            Ok(a as usize)
        }
    }

    fn load(&self, op: u8, a: u64) -> Result<u64, Trap> {
        let n = match op {
            0x28 | 0x2A | 0x34 | 0x35 => 4,
            0x29 | 0x2B => 8,
            0x2C | 0x2D | 0x30 | 0x31 => 1,
            _ => 2,
        };
        let p = self.mem(a, n)?;
        let mut buf = [0u8; 8];
        buf[..n as usize].copy_from_slice(&self.memory[p..p + n as usize]);
        let raw = u64::from_le_bytes(buf);
        Ok(match op {
            0x28 | 0x2A | 0x35 | 0x29 | 0x2B | 0x2D | 0x2F | 0x31 | 0x33 => raw,
            0x2C => raw as u8 as i8 as i32 as u32 as u64,
            0x2E => raw as u16 as i16 as i32 as u32 as u64,
            0x30 => raw as u8 as i8 as i64 as u64,
            0x32 => raw as u16 as i16 as i64 as u64,
            0x34 => raw as u32 as i32 as i64 as u64,
            _ => raw,
        })
    }

    fn store(&mut self, op: u8, a: u64, v: u64) -> Result<(), Trap> {
        let n = match op {
            0x36 | 0x38 | 0x3E => 4,
            0x37 | 0x39 => 8,
            0x3A | 0x3C => 1,
            _ => 2,
        };
        let p = self.mem(a, n)?;
        self.memory[p..p + n as usize].copy_from_slice(&v.to_le_bytes()[..n as usize]);
        Ok(())
    }

    fn numeric(&mut self, op: u8) -> Result<(), Trap> {
        let s = &mut self.stack;
        macro_rules! pop {
            () => {
                s.pop().ok_or(Trap::Unreachable)?
            };
        }
        let b2 = |x: bool| x as u64;
        let v: u64 = match op {
            0x45 => b2(pop!() as u32 == 0),
            0x50 => b2(pop!() == 0),
            0x46..=0x4F => {
                let b = pop!() as u32;
                let a = pop!() as u32;
                let (ai, bi) = (a as i32, b as i32);
                b2(match op {
                    0x46 => a == b,
                    0x47 => a != b,
                    0x48 => ai < bi,
                    0x49 => a < b,
                    0x4A => ai > bi,
                    0x4B => a > b,
                    0x4C => ai <= bi,
                    0x4D => a <= b,
                    0x4E => ai >= bi,
                    _ => a >= b,
                })
            }
            0x51..=0x5A => {
                let b = pop!();
                let a = pop!();
                let (ai, bi) = (a as i64, b as i64);
                b2(match op {
                    0x51 => a == b,
                    0x52 => a != b,
                    0x53 => ai < bi,
                    0x54 => a < b,
                    0x55 => ai > bi,
                    0x56 => a > b,
                    0x57 => ai <= bi,
                    0x58 => a <= b,
                    0x59 => ai >= bi,
                    _ => a >= b,
                })
            }
            0x5B..=0x60 => {
                let b = f32::from_bits(pop!() as u32);
                let a = f32::from_bits(pop!() as u32);
                b2(match op {
                    0x5B => a == b,
                    0x5C => a != b,
                    0x5D => a < b,
                    0x5E => a > b,
                    0x5F => a <= b,
                    _ => a >= b,
                })
            }
            0x61..=0x66 => {
                let b = f64::from_bits(pop!());
                let a = f64::from_bits(pop!());
                b2(match op {
                    0x61 => a == b,
                    0x62 => a != b,
                    0x63 => a < b,
                    0x64 => a > b,
                    0x65 => a <= b,
                    _ => a >= b,
                })
            }
            0x67..=0x69 => {
                let a = pop!() as u32;
                (match op {
                    0x67 => a.leading_zeros(),
                    0x68 => a.trailing_zeros(),
                    _ => a.count_ones(),
                }) as u64
            }
            0x6A..=0x78 => {
                let b = pop!() as u32;
                let a = pop!() as u32;
                let (ai, bi) = (a as i32, b as i32);
                (match op {
                    0x6A => a.wrapping_add(b),
                    0x6B => a.wrapping_sub(b),
                    0x6C => a.wrapping_mul(b),
                    0x6D => {
                        if b == 0 {
                            return Err(Trap::DivZero);
                        }
                        if ai == i32::MIN && bi == -1 {
                            return Err(Trap::Overflow);
                        }
                        (ai / bi) as u32
                    }
                    0x6E => a.checked_div(b).ok_or(Trap::DivZero)?,
                    0x6F => {
                        if b == 0 {
                            return Err(Trap::DivZero);
                        }
                        ai.wrapping_rem(bi) as u32
                    }
                    0x70 => a.checked_rem(b).ok_or(Trap::DivZero)?,
                    0x71 => a & b,
                    0x72 => a | b,
                    0x73 => a ^ b,
                    0x74 => a.wrapping_shl(b),
                    0x75 => ai.wrapping_shr(b) as u32,
                    0x76 => a.wrapping_shr(b),
                    0x77 => a.rotate_left(b),
                    _ => a.rotate_right(b),
                }) as u64
            }
            0x79..=0x7B => {
                let a = pop!();
                (match op {
                    0x79 => a.leading_zeros(),
                    0x7A => a.trailing_zeros(),
                    _ => a.count_ones(),
                }) as u64
            }
            0x7C..=0x8A => {
                let b = pop!();
                let a = pop!();
                let (ai, bi) = (a as i64, b as i64);
                match op {
                    0x7C => a.wrapping_add(b),
                    0x7D => a.wrapping_sub(b),
                    0x7E => a.wrapping_mul(b),
                    0x7F => {
                        if b == 0 {
                            return Err(Trap::DivZero);
                        }
                        if ai == i64::MIN && bi == -1 {
                            return Err(Trap::Overflow);
                        }
                        (ai / bi) as u64
                    }
                    0x80 => a.checked_div(b).ok_or(Trap::DivZero)?,
                    0x81 => {
                        if b == 0 {
                            return Err(Trap::DivZero);
                        }
                        ai.wrapping_rem(bi) as u64
                    }
                    0x82 => a.checked_rem(b).ok_or(Trap::DivZero)?,
                    0x83 => a & b,
                    0x84 => a | b,
                    0x85 => a ^ b,
                    0x86 => a.wrapping_shl(b as u32),
                    0x87 => ai.wrapping_shr(b as u32) as u64,
                    0x88 => a.wrapping_shr(b as u32),
                    0x89 => a.rotate_left((b & 63) as u32),
                    _ => a.rotate_right((b & 63) as u32),
                }
            }
            0x8B..=0x91 => {
                let bits = pop!() as u32;
                let a = f32::from_bits(bits);
                (match op {
                    0x8B => bits & 0x7FFF_FFFF,
                    0x8C => bits ^ 0x8000_0000,
                    0x8D => a.ceil().to_bits(),
                    0x8E => a.floor().to_bits(),
                    0x8F => a.trunc().to_bits(),
                    0x90 => a.round_ties_even().to_bits(),
                    _ => a.sqrt().to_bits(),
                }) as u64
            }
            0x92..=0x98 => {
                let bb = pop!() as u32;
                let ab = pop!() as u32;
                let (a, b) = (f32::from_bits(ab), f32::from_bits(bb));
                (match op {
                    0x92 => (a + b).to_bits(),
                    0x93 => (a - b).to_bits(),
                    0x94 => (a * b).to_bits(),
                    0x95 => (a / b).to_bits(),
                    0x96 => fmin32(a, b),
                    0x97 => fmax32(a, b),
                    _ => (ab & 0x7FFF_FFFF) | (bb & 0x8000_0000),
                }) as u64
            }
            0x99..=0x9F => {
                let bits = pop!();
                let a = f64::from_bits(bits);
                match op {
                    0x99 => bits & 0x7FFF_FFFF_FFFF_FFFF,
                    0x9A => bits ^ 0x8000_0000_0000_0000,
                    0x9B => a.ceil().to_bits(),
                    0x9C => a.floor().to_bits(),
                    0x9D => a.trunc().to_bits(),
                    0x9E => a.round_ties_even().to_bits(),
                    _ => a.sqrt().to_bits(),
                }
            }
            0xA0..=0xA6 => {
                let bb = pop!();
                let ab = pop!();
                let (a, b) = (f64::from_bits(ab), f64::from_bits(bb));
                match op {
                    0xA0 => (a + b).to_bits(),
                    0xA1 => (a - b).to_bits(),
                    0xA2 => (a * b).to_bits(),
                    0xA3 => (a / b).to_bits(),
                    0xA4 => fmin64(a, b),
                    0xA5 => fmax64(a, b),
                    _ => (ab & 0x7FFF_FFFF_FFFF_FFFF) | (bb & 0x8000_0000_0000_0000),
                }
            }
            0xA7 => pop!() as u32 as u64,
            0xA8..=0xAB | 0xAE..=0xB1 => {
                let x = pop!();
                let f = if matches!(op, 0xA8 | 0xA9 | 0xAE | 0xAF) { f32::from_bits(x as u32) as f64 } else { f64::from_bits(x) };
                if f.is_nan() {
                    return Err(Trap::BadConversion);
                }
                let t = f.trunc();
                let (lo, hi) = match op {
                    0xA8 | 0xAA => (-2147483648.0, 2147483648.0),
                    0xA9 | 0xAB => (0.0, 4294967296.0),
                    0xAE | 0xB0 => (-9223372036854775808.0, 9223372036854775808.0),
                    _ => (0.0, 18446744073709551616.0),
                };
                if !(t >= lo && t < hi) {
                    // the spec calls this "integer overflow", but V8 reports NaN and
                    // out-of-range the same way, and that is what we compare against
                    return Err(Trap::BadConversion);
                }
                match op {
                    0xA8 | 0xAA => t as i32 as u32 as u64,
                    0xA9 | 0xAB => t as u32 as u64,
                    0xAE | 0xB0 => t as i64 as u64,
                    _ => t as u64,
                }
            }
            0xAC => pop!() as u32 as i32 as i64 as u64,
            0xAD => pop!() as u32 as u64,
            0xB2 => (pop!() as u32 as i32 as f32).to_bits() as u64,
            0xB3 => (pop!() as u32 as f32).to_bits() as u64,
            0xB4 => (pop!() as i64 as f32).to_bits() as u64,
            0xB5 => (pop!() as f32).to_bits() as u64,
            0xB6 => (f64::from_bits(pop!()) as f32).to_bits() as u64,
            0xB7 => (pop!() as u32 as i32 as f64).to_bits(),
            0xB8 => (pop!() as u32 as f64).to_bits(),
            0xB9 => (pop!() as i64 as f64).to_bits(),
            0xBA => (pop!() as f64).to_bits(),
            0xBB => (f32::from_bits(pop!() as u32) as f64).to_bits(),
            0xBC..=0xBF => pop!(),
            0xC0 => pop!() as u8 as i8 as i32 as u32 as u64,
            0xC1 => pop!() as u16 as i16 as i32 as u32 as u64,
            0xC2 => pop!() as u8 as i8 as i64 as u64,
            0xC3 => pop!() as u16 as i16 as i64 as u64,
            _ => pop!() as u32 as i32 as i64 as u64,
        };
        s.push(v);
        Ok(())
    }
}

fn fmin32(a: f32, b: f32) -> u32 {
    if a.is_nan() || b.is_nan() {
        return 0x7FC0_0000;
    }
    if a == 0.0 && b == 0.0 {
        return a.to_bits() | b.to_bits();
    }
    (if a < b { a } else { b }).to_bits()
}

fn fmax32(a: f32, b: f32) -> u32 {
    if a.is_nan() || b.is_nan() {
        return 0x7FC0_0000;
    }
    if a == 0.0 && b == 0.0 {
        return a.to_bits() & b.to_bits();
    }
    (if a > b { a } else { b }).to_bits()
}

fn fmin64(a: f64, b: f64) -> u64 {
    if a.is_nan() || b.is_nan() {
        return 0x7FF8_0000_0000_0000;
    }
    if a == 0.0 && b == 0.0 {
        return a.to_bits() | b.to_bits();
    }
    (if a < b { a } else { b }).to_bits()
}

fn fmax64(a: f64, b: f64) -> u64 {
    if a.is_nan() || b.is_nan() {
        return 0x7FF8_0000_0000_0000;
    }
    if a == 0.0 && b == 0.0 {
        return a.to_bits() & b.to_bits();
    }
    (if a > b { a } else { b }).to_bits()
}

/// Saturating float-to-int: Rust's `as` already saturates and maps NaN to 0.
fn trunc_sat(n: u8, x: u64) -> u64 {
    let f32v = || f32::from_bits(x as u32);
    let f64v = || f64::from_bits(x);
    match n {
        0 => f32v() as i32 as u32 as u64,
        1 => f32v() as u32 as u64,
        2 => f64v() as i32 as u32 as u64,
        3 => f64v() as u32 as u64,
        4 => f32v() as i64 as u64,
        5 => f32v() as u64,
        6 => f64v() as i64 as u64,
        _ => f64v() as u64,
    }
}

/// Format a result the way tools/run.mjs prints V8's: NaNs of either width
/// print as "nan" because their payload isn't observable through JS.
pub fn format_value(t: ValType, v: u64) -> String {
    match t {
        ValType::I32 => format!("i32:{}", v as u32 as i32),
        ValType::I64 => format!("i64:{}", v as i64),
        ValType::F32 => {
            if f32::from_bits(v as u32).is_nan() {
                "f32:nan".into()
            } else {
                format!("f32:0x{:08x}", v as u32)
            }
        }
        ValType::F64 => {
            if f64::from_bits(v).is_nan() {
                "f64:nan".into()
            } else {
                format!("f64:0x{:016x}", v)
            }
        }
    }
}
