//! Random valid programs for differential testing against V8.
//!
//! Every module has one page of memory (max two), a few globals, a chain of
//! functions where each may call only earlier ones (so nothing recurses), and
//! a `checksum` export that hashes all of memory. Expressions are generated
//! by type, so the module is always valid. Integer division, trapping
//! float-to-int conversion, out-of-range loads and stores all appear, so traps
//! are compared too.
//!
//! NaN payloads and signs are not deterministic in WebAssembly (V8 on x86 and
//! Rust on ARM produce different NaN bits for 0/0), so every float whose bits
//! become observable (reinterpret, store, copysign's sign) goes through
//! `canon` first.

use crate::build::{Builder, Code, Func};
use crate::module::ValType::{self, *};

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

const TYPES: [ValType; 4] = [I32, I64, F32, F64];

struct Fn<'a> {
    r: &'a mut Rng,
    c: Code,
    locals: Vec<ValType>,
    callees: &'a [(u32, Vec<ValType>, ValType)],
    globals: &'a [ValType],
    counter: u32,
    in_loop: bool,
    budget: i32,
}

pub fn interesting(r: &mut Rng, t: ValType) -> u64 {
    let (small, any) = (r.below(1000), r.next());
    match t {
        I32 => r.pick(&[0u64, 1, 0xFFFF_FFFF, 0x8000_0000, 0x7FFF_FFFF, 31, 32, 7, small % 100, any & 0xFFFF_FFFF]),
        I64 => r.pick(&[0u64, 1, u64::MAX, 1 << 63, (1 << 63) - 1, 63, 64, 1 << 32, small, any]),
        F32 => r.pick(&[
            0u64, 0x8000_0000, 0x3F80_0000, 0xBFC0_0000, 0x7F80_0000, 0xFF80_0000, 0x7F7F_FFFF, 0x0000_0001, 0x4F00_0000,
            0x4F80_0000, 0xCF00_0001, 0x3F00_0000, 0x4020_0000, ((small as f32 - 500.0) * 1.25).to_bits() as u64,
            any & 0xFFFF_FFFF,
        ]),
        F64 => r.pick(&[
            0u64, 1 << 63, 0x3FF0_0000_0000_0000, 0xBFF8_0000_0000_0000, 0x7FF0_0000_0000_0000, 0xFFF0_0000_0000_0000,
            0x7FEF_FFFF_FFFF_FFFF, 1, 0x41E0_0000_0000_0000, 0x43E0_0000_0000_0000, 0xC3E0_0000_0000_0001, 0x4004_0000_0000_0000,
            (small as f64 / 7.0 - 70.0).to_bits(), any,
        ]),
    }
}

impl<'a> Fn<'a> {
    fn konst(&mut self, t: ValType) {
        let v = interesting(self.r, t);
        match t {
            I32 => self.c.i32c(v as u32 as i32),
            I64 => self.c.i64c(v as i64),
            F32 => self.c.f32c(v as u32),
            F64 => self.c.f64c(v),
        };
    }

    fn local_of(&mut self, t: ValType) -> Option<u32> {
        let idx: Vec<u32> = (0..self.locals.len() as u32).filter(|&i| self.locals[i as usize] == t && i != self.counter).collect();
        if idx.is_empty() {
            None
        } else {
            Some(self.r.pick(&idx))
        }
    }

    /// Replace a NaN with the canonical NaN so its bits are deterministic.
    fn canon(&mut self, t: ValType) {
        let tmp = self.local_of(t);
        let Some(tmp) = tmp else {
            return;
        };
        let (nan, eq) = if t == F32 { (0x7FC0_0000u64, 0x5B) } else { (0x7FF8_0000_0000_0000u64, 0x61) };
        self.c.tee(tmp);
        if t == F32 {
            self.c.f32c(nan as u32);
        } else {
            self.c.f64c(nan);
        }
        self.c.get(tmp).get(tmp).op(eq).op(0x1B);
    }

    fn addr(&mut self) {
        self.expr(I32, 2);
        if self.r.chance(0.97) {
            self.c.i32c(0xFFF8).op(0x71);
        }
    }

    fn expr(&mut self, t: ValType, depth: u32) {
        self.budget -= 1;
        if depth == 0 || self.budget <= 0 || self.r.chance(0.2) {
            if self.r.chance(0.5) {
                if let Some(l) = self.local_of(t) {
                    self.c.get(l);
                    return;
                }
            }
            if self.r.chance(0.2) {
                let gs: Vec<u32> = (0..self.globals.len() as u32).filter(|&g| self.globals[g as usize] == t).collect();
                if !gs.is_empty() {
                    let g = self.r.pick(&gs);
                    self.c.op(0x23).u(g);
                    return;
                }
            }
            self.konst(t);
            return;
        }
        let d = depth - 1;
        match self.r.below(13) {
            0 | 1 | 2 => self.binary(t, d),
            3 | 4 => self.unary(t, d),
            5 => self.convert(t, d),
            6 => {
                // select
                self.expr(t, d);
                self.expr(t, d);
                self.expr(I32, d);
                self.c.op(0x1B);
            }
            7 => {
                self.expr(I32, d);
                self.c.block(0x04, Some(t));
                self.expr(t, d);
                self.c.op(0x05);
                self.expr(t, d);
                self.c.op(0x0B);
            }
            8 => {
                // block with a conditional early exit carrying a value
                self.c.block(0x02, Some(t));
                self.stmt(d);
                self.expr(t, d);
                self.expr(I32, d);
                self.c.op(0x0D).u(0);
                self.c.op(0x1A);
                self.expr(t, d);
                self.c.op(0x0B);
            }
            9 => self.load(t, d),
            10 => {
                if let Some(l) = self.local_of(t) {
                    self.expr(t, d);
                    self.c.tee(l);
                } else {
                    self.konst(t);
                }
            }
            11 => {
                let cs: Vec<usize> = (0..self.callees.len()).filter(|&i| self.callees[i].2 == t).collect();
                if cs.is_empty() {
                    self.binary(t, d);
                } else {
                    let pick = self.r.pick(&cs);
                    let (f, params, _) = self.callees[pick].clone();
                    for p in params {
                        self.expr(p, d.min(2));
                    }
                    self.c.op(0x10).u(f);
                }
            }
            _ => {
                if !self.in_loop && t != I32 {
                    self.lp(t, d);
                } else {
                    self.binary(t, d);
                }
            }
        }
    }

    fn binary(&mut self, t: ValType, d: u32) {
        if t == I32 && self.r.chance(0.3) {
            // comparison of any type
            let (ot, lo, hi) = self.r.pick(&[(I32, 0x46u8, 0x4Fu8), (I64, 0x51, 0x5A), (F32, 0x5B, 0x60), (F64, 0x61, 0x66)]);
            self.expr(ot, d);
            self.expr(ot, d);
            let op = lo + self.r.below((hi - lo + 1) as u64) as u8;
            self.c.op(op);
            return;
        }
        let (lo, hi) = match t {
            I32 => (0x6A, 0x78),
            I64 => (0x7C, 0x8A),
            F32 => (0x92, 0x98),
            F64 => (0xA0, 0xA6),
        };
        let op = lo + self.r.below((hi - lo + 1) as u64) as u8;
        self.expr(t, d);
        self.expr(t, d);
        if op == 0x98 || op == 0xA6 {
            self.canon(t); // copysign reads the sign of its second operand
        }
        self.c.op(op);
    }

    fn unary(&mut self, t: ValType, d: u32) {
        let ops: &[u8] = match t {
            I32 => &[0x67, 0x68, 0x69, 0x45, 0xC0, 0xC1, 0x50],
            I64 => &[0x79, 0x7A, 0x7B, 0xC2, 0xC3, 0xC4],
            F32 => &[0x8B, 0x8C, 0x8D, 0x8E, 0x8F, 0x90, 0x91],
            F64 => &[0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F],
        };
        let op = self.r.pick(ops);
        self.expr(if op == 0x50 { I64 } else { t }, d);
        self.c.op(op);
    }

    fn convert(&mut self, t: ValType, d: u32) {
        // (source type, opcode, prefixed with 0xFC)
        let choices: &[(ValType, u8, bool)] = match t {
            I32 => &[(I64, 0xA7, false), (F32, 0xA8, false), (F32, 0xA9, false), (F64, 0xAA, false), (F64, 0xAB, false), (F32, 0xBC, false), (F32, 0, true), (F32, 1, true), (F64, 2, true), (F64, 3, true)],
            I64 => &[(I32, 0xAC, false), (I32, 0xAD, false), (F32, 0xAE, false), (F32, 0xAF, false), (F64, 0xB0, false), (F64, 0xB1, false), (F64, 0xBD, false), (F32, 4, true), (F32, 5, true), (F64, 6, true), (F64, 7, true)],
            F32 => &[(I32, 0xB2, false), (I32, 0xB3, false), (I64, 0xB4, false), (I64, 0xB5, false), (F64, 0xB6, false), (I32, 0xBE, false)],
            F64 => &[(I32, 0xB7, false), (I32, 0xB8, false), (I64, 0xB9, false), (I64, 0xBA, false), (F32, 0xBB, false), (I64, 0xBF, false)],
        };
        let (src, op, fc) = self.r.pick(choices);
        self.expr(src, d);
        if op == 0xBC || op == 0xBD {
            self.canon(src);
        }
        if fc {
            self.c.op(0xFC).u(op as u32);
        } else {
            self.c.op(op);
        }
    }

    fn load(&mut self, t: ValType, _d: u32) {
        let ops: &[u8] = match t {
            I32 => &[0x28, 0x2C, 0x2D, 0x2E, 0x2F],
            I64 => &[0x29, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35],
            F32 => &[0x2A],
            F64 => &[0x2B],
        };
        let op = self.r.pick(ops);
        self.addr();
        let off = self.r.pick(&[0u32, 0, 1, 4, 8, 3]);
        self.c.mem(op, off);
    }

    fn stmt(&mut self, d: u32) {
        match self.r.below(5) {
            0 | 1 => {
                let t = self.r.pick(&TYPES);
                let ops: &[u8] = match t {
                    I32 => &[0x36, 0x3A, 0x3B],
                    I64 => &[0x37, 0x3C, 0x3D, 0x3E],
                    F32 => &[0x38],
                    F64 => &[0x39],
                };
                let op = self.r.pick(ops);
                self.addr();
                self.expr(t, d);
                if t == F32 || t == F64 {
                    self.canon(t);
                }
                let off = self.r.pick(&[0u32, 0, 2, 8]);
                self.c.mem(op, off);
            }
            2 => {
                let gs: Vec<u32> = (0..self.globals.len() as u32).collect();
                if !gs.is_empty() {
                    let g = self.r.pick(&gs);
                    let t = self.globals[g as usize];
                    self.expr(t, d);
                    self.c.op(0x24).u(g);
                }
            }
            3 => {
                let t = self.r.pick(&TYPES);
                if let Some(l) = self.local_of(t) {
                    self.expr(t, d);
                    self.c.set(l);
                }
            }
            _ => {
                if self.r.chance(0.1) {
                    let n = self.r.below(2) as i32;
                    self.c.i32c(n).op(0x40).op(0).op(0x1A); // memory.grow
                }
            }
        }
    }

    /// A bounded loop that folds a value into an accumulator.
    fn lp(&mut self, t: ValType, d: u32) {
        let Some(acc) = self.local_of(t) else {
            self.konst(t);
            return;
        };
        self.in_loop = true;
        let n = 1 + self.r.below(12) as i32;
        self.c.i32c(n).set(self.counter);
        self.c.block(0x03, None);
        self.stmt(d);
        self.expr(t, d);
        self.c.set(acc);
        self.c.get(self.counter).i32c(1).op(0x6B).tee(self.counter).op(0x0D).u(0);
        self.c.op(0x0B);
        self.c.get(acc);
        self.in_loop = false;
    }
}

/// A random module and the calls to make on it, one per line ("f2 i32:5 f64:0x...").
pub fn module(seed: u64) -> (Vec<u8>, String) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut b = Builder { memory: Some((1, Some(2))), ..Default::default() };
    let nglobals = r.below(4) as usize;
    let gtypes: Vec<ValType> = (0..nglobals).map(|_| r.pick(&TYPES)).collect();
    for &t in &gtypes {
        let v = interesting(&mut r, t);
        b.globals.push((t, true, v));
    }
    let mut data = vec![0u8; 64];
    for x in data.iter_mut() {
        *x = r.next() as u8;
    }
    b.data.push((r.below(60000) as u32, data));
    let nfuncs = 2 + r.below(5) as u32;
    let mut sigs: Vec<(u32, Vec<ValType>, ValType)> = Vec::new();
    let mut calls = String::new();
    for fi in 0..nfuncs {
        let params: Vec<ValType> = (0..r.below(4)).map(|_| r.pick(&TYPES)).collect();
        let result = r.pick(&TYPES);
        let mut locals: Vec<ValType> = params.clone();
        let extra: Vec<ValType> = (0..2 + r.below(4)).map(|_| r.pick(&TYPES)).chain(TYPES).collect();
        locals.extend(&extra);
        locals.push(I32);
        let counter = locals.len() as u32 - 1;
        let callees = sigs.clone();
        let mut f = Fn { r: &mut r, c: Code::default(), locals, callees: &callees, globals: &gtypes, counter, in_loop: false, budget: 60 };
        for _ in 0..f.r.below(3) {
            f.stmt(3);
        }
        f.expr(result, 5);
        let code = f.c;
        b.funcs.push(Func { params: params.clone(), results: vec![result], locals: extra.into_iter().chain([I32]).collect(), code });
        b.exports.push((format!("f{}", fi), fi));
        sigs.push((fi, params.clone(), result));
        for _ in 0..2 {
            let args: Vec<String> = params.iter().map(|&t| format_arg(t, interesting(&mut r, t))).collect();
            calls.push_str(&format!("f{} {}\n", fi, args.join(" ")));
            calls.push_str("checksum\n");
        }
    }
    // checksum: () -> i64, hash of all of memory, 8 bytes at a time
    let mut c = Code::default();
    c.op(0x3F).op(0).i32c(16).op(0x74).set(0); // words = pages << 16 bytes
    c.block(0x02, None).block(0x03, None);
    c.get(0).op(0x45).op(0x0D).u(1);
    c.get(0).i32c(8).op(0x6B).tee(0);
    c.mem(0x29, 0).get(1).i64c(5).op(0x89).op(0x85).set(1);
    c.op(0x0C).u(0).op(0x0B).op(0x0B);
    c.get(1);
    let ci = b.funcs.len() as u32;
    b.funcs.push(Func { params: vec![], results: vec![I64], locals: vec![I32, I64], code: c });
    b.exports.push(("checksum".into(), ci));
    (b.encode(), calls)
}

pub fn format_arg(t: ValType, v: u64) -> String {
    match t {
        I32 => format!("i32:{}", v as u32 as i32),
        I64 => format!("i64:{}", v as i64),
        // NaN arguments would lose their payload on the way through JS
        F32 => format!("f32:0x{:08x}", if f32::from_bits(v as u32).is_nan() { 0 } else { v as u32 }),
        F64 => format!("f64:0x{:016x}", if f64::from_bits(v).is_nan() { 0 } else { v }),
    }
}
