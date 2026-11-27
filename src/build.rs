//! Encoding modules: used by the program generator and the examples.

use crate::module::ValType;

pub fn uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        out.push(if done { b } else { b | 0x80 });
        if done {
            return;
        }
    }
}

pub fn vt(t: ValType) -> u8 {
    match t {
        ValType::I32 => 0x7F,
        ValType::I64 => 0x7E,
        ValType::F32 => 0x7D,
        ValType::F64 => 0x7C,
    }
}

/// A function body under construction.
#[derive(Default, Clone)]
pub struct Code(pub Vec<u8>);

impl Code {
    pub fn op(&mut self, b: u8) -> &mut Self {
        self.0.push(b);
        self
    }
    pub fn u(&mut self, v: u32) -> &mut Self {
        uleb(&mut self.0, v as u64);
        self
    }
    pub fn i32c(&mut self, v: i32) -> &mut Self {
        self.0.push(0x41);
        sleb(&mut self.0, v as i64);
        self
    }
    pub fn i64c(&mut self, v: i64) -> &mut Self {
        self.0.push(0x42);
        sleb(&mut self.0, v);
        self
    }
    pub fn f32c(&mut self, bits: u32) -> &mut Self {
        self.0.push(0x43);
        self.0.extend_from_slice(&bits.to_le_bytes());
        self
    }
    pub fn f64c(&mut self, bits: u64) -> &mut Self {
        self.0.push(0x44);
        self.0.extend_from_slice(&bits.to_le_bytes());
        self
    }
    pub fn get(&mut self, i: u32) -> &mut Self {
        self.op(0x20).u(i)
    }
    pub fn set(&mut self, i: u32) -> &mut Self {
        self.op(0x21).u(i)
    }
    pub fn tee(&mut self, i: u32) -> &mut Self {
        self.op(0x22).u(i)
    }
    pub fn mem(&mut self, op: u8, offset: u32) -> &mut Self {
        self.op(op).u(0).u(offset)
    }
    pub fn block(&mut self, op: u8, result: Option<ValType>) -> &mut Self {
        self.op(op).op(result.map_or(0x40, vt))
    }
}

pub struct Func {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
    pub locals: Vec<ValType>,
    pub code: Code,
}

#[derive(Default)]
pub struct Builder {
    pub funcs: Vec<Func>,
    pub memory: Option<(u32, Option<u32>)>,
    pub table: Vec<u32>,
    pub globals: Vec<(ValType, bool, u64)>,
    pub exports: Vec<(String, u32)>,
    pub data: Vec<(u32, Vec<u8>)>,
}

fn section(out: &mut Vec<u8>, id: u8, body: Vec<u8>) {
    out.push(id);
    uleb(out, body.len() as u64);
    out.extend(body);
}

impl Builder {
    pub fn encode(&self) -> Vec<u8> {
        let mut types: Vec<(Vec<ValType>, Vec<ValType>)> = Vec::new();
        let mut func_type = Vec::new();
        for f in &self.funcs {
            let key = (f.params.clone(), f.results.clone());
            let i = types.iter().position(|t| *t == key).unwrap_or_else(|| {
                types.push(key);
                types.len() - 1
            });
            func_type.push(i as u32);
        }
        let mut out = b"\0asm\x01\0\0\0".to_vec();
        let mut s = Vec::new();
        uleb(&mut s, types.len() as u64);
        for (p, r) in &types {
            s.push(0x60);
            uleb(&mut s, p.len() as u64);
            s.extend(p.iter().map(|&t| vt(t)));
            uleb(&mut s, r.len() as u64);
            s.extend(r.iter().map(|&t| vt(t)));
        }
        section(&mut out, 1, s);
        let mut s = Vec::new();
        uleb(&mut s, func_type.len() as u64);
        for t in &func_type {
            uleb(&mut s, *t as u64);
        }
        section(&mut out, 3, s);
        if !self.table.is_empty() {
            let mut s = vec![1, 0x70, 0];
            uleb(&mut s, self.table.len() as u64);
            section(&mut out, 4, s);
        }
        if let Some((min, max)) = self.memory {
            let mut s = vec![1];
            match max {
                None => {
                    s.push(0);
                    uleb(&mut s, min as u64);
                }
                Some(m) => {
                    s.push(1);
                    uleb(&mut s, min as u64);
                    uleb(&mut s, m as u64);
                }
            }
            section(&mut out, 5, s);
        }
        if !self.globals.is_empty() {
            let mut s = Vec::new();
            uleb(&mut s, self.globals.len() as u64);
            for (t, m, init) in &self.globals {
                s.push(vt(*t));
                s.push(*m as u8);
                let mut c = Code::default();
                match t {
                    ValType::I32 => c.i32c(*init as i32),
                    ValType::I64 => c.i64c(*init as i64),
                    ValType::F32 => c.f32c(*init as u32),
                    ValType::F64 => c.f64c(*init),
                };
                s.extend(c.0);
                s.push(0x0B);
            }
            section(&mut out, 6, s);
        }
        let mut s = Vec::new();
        uleb(&mut s, self.exports.len() as u64);
        for (name, f) in &self.exports {
            uleb(&mut s, name.len() as u64);
            s.extend_from_slice(name.as_bytes());
            s.push(0);
            uleb(&mut s, *f as u64);
        }
        section(&mut out, 7, s);
        if !self.table.is_empty() {
            let mut s = vec![1, 0, 0x41, 0, 0x0B];
            uleb(&mut s, self.table.len() as u64);
            for f in &self.table {
                uleb(&mut s, *f as u64);
            }
            section(&mut out, 9, s);
        }
        let mut s = Vec::new();
        uleb(&mut s, self.funcs.len() as u64);
        for f in &self.funcs {
            let mut body = Vec::new();
            // run-length encode local types
            let mut groups: Vec<(u32, ValType)> = Vec::new();
            for &t in &f.locals {
                match groups.last_mut() {
                    Some((n, lt)) if *lt == t => *n += 1,
                    _ => groups.push((1, t)),
                }
            }
            uleb(&mut body, groups.len() as u64);
            for (n, t) in groups {
                uleb(&mut body, n as u64);
                body.push(vt(t));
            }
            body.extend_from_slice(&f.code.0);
            body.push(0x0B);
            uleb(&mut s, body.len() as u64);
            s.extend(body);
        }
        section(&mut out, 10, s);
        if !self.data.is_empty() {
            let mut s = Vec::new();
            uleb(&mut s, self.data.len() as u64);
            for (off, bytes) in &self.data {
                s.push(0);
                let mut c = Code::default();
                c.i32c(*off as i32);
                s.extend(c.0);
                s.push(0x0B);
                uleb(&mut s, bytes.len() as u64);
                s.extend_from_slice(bytes);
            }
            section(&mut out, 11, s);
        }
        out
    }
}
