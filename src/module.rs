//! Decoding a WebAssembly binary into a Module.
//!
//! Covers the MVP plus sign-extension and saturating float-to-int ops: types,
//! functions, one table of funcrefs, one memory, globals, exports, start,
//! active element and data segments. Function bodies are decoded into a flat
//! list of Ops with every block's matching else/end resolved up front.
//!
//! Decoding checks that the binary is well formed and that indices are in
//! range, so execution never panics. It does NOT type-check function bodies
//! (operand stack types, block results, br depths against value types).
//! That is the validator's job and kiln doesn't have one yet.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
}

impl ValType {
    fn from_byte(b: u8) -> Result<ValType, DecodeError> {
        Ok(match b {
            0x7F => ValType::I32,
            0x7E => ValType::I64,
            0x7D => ValType::F32,
            0x7C => ValType::F64,
            _ => return Err(DecodeError(format!("bad value type 0x{:02x}", b))),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Unreachable,
    Nop,
    /// end: index of the matching End; arity: number of results (0 or 1)
    Block { end: u32, arity: u8 },
    Loop,
    If { else_: u32, end: u32, arity: u8 },
    Else { end: u32 },
    End,
    Br(u32),
    BrIf(u32),
    /// index into Func::br_tables
    BrTable(u32),
    Return,
    Call(u32),
    CallIndirect(u32),
    Drop,
    Select,
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),
    Load { op: u8, offset: u32 },
    Store { op: u8, offset: u32 },
    MemorySize,
    MemoryGrow,
    I32Const(i32),
    I64Const(i64),
    F32Const(u32),
    F64Const(u64),
    /// any numeric opcode 0x45..=0xC4
    Num(u8),
    /// 0xFC prefix: saturating truncations 0..=7
    TruncSat(u8),
}

#[derive(Debug, Clone)]
pub struct Func {
    pub type_idx: u32,
    /// local types after the params
    pub locals: Vec<ValType>,
    pub code: Vec<Op>,
    pub br_tables: Vec<(Vec<u32>, u32)>,
}

#[derive(Debug, Clone)]
pub struct Global {
    pub ty: ValType,
    pub mutable: bool,
    pub init: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportKind {
    Func,
    Table,
    Memory,
    Global,
}

#[derive(Debug, Clone, Default)]
pub struct Module {
    pub types: Vec<FuncType>,
    pub funcs: Vec<Func>,
    pub table: Option<(u32, Option<u32>)>,
    pub memory: Option<(u32, Option<u32>)>,
    pub globals: Vec<Global>,
    pub exports: Vec<(String, ExportKind, u32)>,
    pub start: Option<u32>,
    /// (table offset, function indices)
    pub elems: Vec<(u32, Vec<u32>)>,
    /// (memory offset, bytes)
    pub data: Vec<(u32, Vec<u8>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(m: impl Into<String>) -> Result<T, DecodeError> {
    Err(DecodeError(m.into()))
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let v = *self.b.get(self.p).ok_or_else(|| DecodeError("unexpected end".into()))?;
        self.p += 1;
        Ok(v)
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.p + n > self.b.len() {
            return err("unexpected end");
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        let mut result: u64 = 0;
        for i in 0..5 {
            let b = self.byte()?;
            result |= ((b & 0x7F) as u64) << (7 * i);
            if b & 0x80 == 0 {
                if result > u32::MAX as u64 {
                    return err("integer too large");
                }
                return Ok(result as u32);
            }
        }
        err("integer representation too long")
    }
    fn sleb(&mut self, bits: u32) -> Result<i64, DecodeError> {
        let mut result: i64 = 0;
        let mut shift = 0;
        let max_bytes = (bits + 6) / 7;
        for _ in 0..max_bytes {
            let b = self.byte()?;
            result |= ((b & 0x7F) as i64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && b & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                if bits < 64 {
                    let min = -(1i64 << (bits - 1));
                    let max = (1i64 << (bits - 1)) - 1;
                    if result < min || result > max {
                        return err("integer too large");
                    }
                }
                return Ok(result);
            }
        }
        err("integer representation too long")
    }
    fn name(&mut self) -> Result<String, DecodeError> {
        let n = self.u32()? as usize;
        String::from_utf8(self.bytes(n)?.to_vec()).map_err(|_| DecodeError("malformed UTF-8".into()))
    }
    fn limits(&mut self) -> Result<(u32, Option<u32>), DecodeError> {
        match self.byte()? {
            0 => Ok((self.u32()?, None)),
            1 => {
                let min = self.u32()?;
                let max = self.u32()?;
                if max < min {
                    return err("size minimum must not be greater than maximum");
                }
                Ok((min, Some(max)))
            }
            _ => err("bad limits flag"),
        }
    }
    fn const_expr(&mut self, globals: &[Global]) -> Result<(ValType, u64), DecodeError> {
        let v = match self.byte()? {
            0x41 => (ValType::I32, self.sleb(32)? as i32 as u32 as u64),
            0x42 => (ValType::I64, self.sleb(64)? as u64),
            0x43 => (ValType::F32, u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()) as u64),
            0x44 => (ValType::F64, u64::from_le_bytes(self.bytes(8)?.try_into().unwrap())),
            0x23 => {
                let i = self.u32()? as usize;
                let g = globals.get(i).ok_or_else(|| DecodeError("unknown global".into()))?;
                (g.ty, g.init)
            }
            op => return err(format!("unsupported constant expression op 0x{:02x}", op)),
        };
        if self.byte()? != 0x0B {
            return err("constant expression must end after one instruction");
        }
        Ok(v)
    }
}

pub fn decode(bytes: &[u8]) -> Result<Module, DecodeError> {
    let mut r = Reader { b: bytes, p: 0 };
    if r.bytes(4)? != b"\0asm" {
        return err("magic header not detected");
    }
    if r.bytes(4)? != [1, 0, 0, 0] {
        return err("unknown binary version");
    }
    let mut m = Module::default();
    let mut func_types: Vec<u32> = Vec::new();
    let mut bodies: Vec<(Vec<ValType>, &[u8])> = Vec::new();
    while r.p < bytes.len() {
        let id = r.byte()?;
        let size = r.u32()? as usize;
        let body = r.bytes(size)?;
        let mut s = Reader { b: body, p: 0 };
        match id {
            0 => {
                s.name()?;
                continue;
            }
            1 => {
                for _ in 0..s.u32()? {
                    if s.byte()? != 0x60 {
                        return err("bad function type");
                    }
                    let np = s.u32()?;
                    let params = (0..np).map(|_| s.byte().and_then(ValType::from_byte)).collect::<Result<_, _>>()?;
                    let nr = s.u32()?;
                    let results: Vec<ValType> = (0..nr).map(|_| s.byte().and_then(ValType::from_byte)).collect::<Result<_, _>>()?;
                    if results.len() > 1 {
                        return err("multiple results are not supported");
                    }
                    m.types.push(FuncType { params, results });
                }
            }
            2 => {
                if s.u32()? > 0 {
                    return err("imports are not supported");
                }
            }
            3 => {
                for _ in 0..s.u32()? {
                    let t = s.u32()?;
                    if t as usize >= m.types.len() {
                        return err("unknown type");
                    }
                    func_types.push(t);
                }
            }
            4 => {
                for _ in 0..s.u32()? {
                    if s.byte()? != 0x70 {
                        return err("only funcref tables are supported");
                    }
                    if m.table.is_some() {
                        return err("multiple tables");
                    }
                    m.table = Some(s.limits()?);
                }
            }
            5 => {
                for _ in 0..s.u32()? {
                    if m.memory.is_some() {
                        return err("multiple memories");
                    }
                    let l = s.limits()?;
                    if l.0 > 65536 || l.1.map_or(false, |x| x > 65536) {
                        return err("memory size must be at most 65536 pages (4GiB)");
                    }
                    m.memory = Some(l);
                }
            }
            6 => {
                for _ in 0..s.u32()? {
                    let ty = ValType::from_byte(s.byte()?)?;
                    let mutable = match s.byte()? {
                        0 => false,
                        1 => true,
                        _ => return err("malformed mutability"),
                    };
                    let (_, init) = s.const_expr(&m.globals)?;
                    m.globals.push(Global { ty, mutable, init });
                }
            }
            7 => {
                for _ in 0..s.u32()? {
                    let name = s.name()?;
                    let kind = match s.byte()? {
                        0 => ExportKind::Func,
                        1 => ExportKind::Table,
                        2 => ExportKind::Memory,
                        3 => ExportKind::Global,
                        _ => return err("bad export kind"),
                    };
                    let idx = s.u32()?;
                    m.exports.push((name, kind, idx));
                }
            }
            8 => m.start = Some(s.u32()?),
            9 => {
                for _ in 0..s.u32()? {
                    if s.u32()? != 0 {
                        return err("only active element segments for table 0 are supported");
                    }
                    let (_, off) = s.const_expr(&m.globals)?;
                    let n = s.u32()?;
                    let fs = (0..n).map(|_| s.u32()).collect::<Result<_, _>>()?;
                    m.elems.push((off as u32, fs));
                }
            }
            10 => {
                let n = s.u32()?;
                for _ in 0..n {
                    let size = s.u32()? as usize;
                    let code = s.bytes(size)?;
                    let mut c = Reader { b: code, p: 0 };
                    let mut locals = Vec::new();
                    for _ in 0..c.u32()? {
                        let count = c.u32()?;
                        let ty = ValType::from_byte(c.byte()?)?;
                        if locals.len() as u64 + count as u64 > 50_000 {
                            return err("too many locals");
                        }
                        locals.extend(std::iter::repeat(ty).take(count as usize));
                    }
                    bodies.push((locals, &code[c.p..]));
                }
            }
            11 => {
                for _ in 0..s.u32()? {
                    if s.u32()? != 0 {
                        return err("only active data segments for memory 0 are supported");
                    }
                    let (_, off) = s.const_expr(&m.globals)?;
                    let n = s.u32()? as usize;
                    m.data.push((off as u32, s.bytes(n)?.to_vec()));
                }
            }
            12 => {
                s.u32()?;
            }
            _ => return err(format!("unknown section id {}", id)),
        }
        if s.p != body.len() {
            return err("section size mismatch");
        }
    }
    if func_types.len() != bodies.len() {
        return err("function and code section have inconsistent lengths");
    }
    let nfuncs = func_types.len() as u32;
    for (t, (locals, code)) in func_types.iter().zip(bodies) {
        let nlocals = m.types[*t as usize].params.len() + locals.len();
        let (code, br_tables) = decode_body(code, nlocals as u32, nfuncs, &m)?;
        m.funcs.push(Func { type_idx: *t, locals, code, br_tables });
    }
    for (_, kind, idx) in &m.exports {
        let ok = match kind {
            ExportKind::Func => *idx < nfuncs,
            ExportKind::Table => m.table.is_some() && *idx == 0,
            ExportKind::Memory => m.memory.is_some() && *idx == 0,
            ExportKind::Global => (*idx as usize) < m.globals.len(),
        };
        if !ok {
            return err("unknown export index");
        }
    }
    if m.start.map_or(false, |s| s >= nfuncs) {
        return err("unknown function");
    }
    for (_, fs) in &m.elems {
        if m.table.is_none() || fs.iter().any(|&f| f >= nfuncs) {
            return err("unknown function or table");
        }
    }
    if !m.data.is_empty() && m.memory.is_none() {
        return err("unknown memory");
    }
    Ok(m)
}

fn decode_body(code: &[u8], nlocals: u32, nfuncs: u32, m: &Module) -> Result<(Vec<Op>, Vec<(Vec<u32>, u32)>), DecodeError> {
    let mut r = Reader { b: code, p: 0 };
    let mut ops = Vec::new();
    let mut tables = Vec::new();
    // open blocks: index of the Block/Loop/If op
    let mut open: Vec<usize> = Vec::new();
    let block_arity = |r: &mut Reader| -> Result<u8, DecodeError> {
        match r.byte()? {
            0x40 => Ok(0),
            b => {
                ValType::from_byte(b).map_err(|_| DecodeError("multi-value block types are not supported".into()))?;
                Ok(1)
            }
        }
    };
    let mem = |m: &Module| if m.memory.is_some() { Ok(()) } else { err("unknown memory") };
    loop {
        let op = r.byte()?;
        let o = match op {
            0x00 => Op::Unreachable,
            0x01 => Op::Nop,
            0x02 => {
                open.push(ops.len());
                Op::Block { end: 0, arity: block_arity(&mut r)? }
            }
            0x03 => {
                block_arity(&mut r)?;
                open.push(ops.len());
                Op::Loop
            }
            0x04 => {
                open.push(ops.len());
                Op::If { else_: 0, end: 0, arity: block_arity(&mut r)? }
            }
            0x05 => {
                let Some(&start) = open.last() else { return err("else without if") };
                let here = ops.len() as u32;
                match &mut ops[start] {
                    Op::If { else_, .. } if *else_ == 0 => *else_ = here,
                    _ => return err("else without if"),
                }
                Op::Else { end: 0 }
            }
            0x0B => {
                match open.pop() {
                    None => {
                        ops.push(Op::End);
                        break;
                    }
                    Some(start) => {
                        let end = ops.len() as u32;
                        let mut else_at = None;
                        match &mut ops[start] {
                            Op::Block { end: e, .. } => *e = end,
                            Op::If { else_, end: e, .. } => {
                                *e = end;
                                if *else_ != 0 {
                                    else_at = Some(*else_ as usize);
                                }
                            }
                            _ => {}
                        }
                        if let Some(i) = else_at {
                            if let Op::Else { end: ee } = &mut ops[i] {
                                *ee = end;
                            }
                        }
                        Op::End
                    }
                }
            }
            0x0C => Op::Br(r.u32()?),
            0x0D => Op::BrIf(r.u32()?),
            0x0E => {
                let n = r.u32()?;
                if n > 100_000 {
                    return err("br_table too large");
                }
                let labels = (0..n).map(|_| r.u32()).collect::<Result<_, _>>()?;
                let default = r.u32()?;
                tables.push((labels, default));
                Op::BrTable(tables.len() as u32 - 1)
            }
            0x0F => Op::Return,
            0x10 => {
                let f = r.u32()?;
                if f >= nfuncs {
                    return err("unknown function");
                }
                Op::Call(f)
            }
            0x11 => {
                let t = r.u32()?;
                if r.byte()? != 0 || m.table.is_none() {
                    return err("unknown table");
                }
                if t as usize >= m.types.len() {
                    return err("unknown type");
                }
                Op::CallIndirect(t)
            }
            0x1A => Op::Drop,
            0x1B => Op::Select,
            0x20 | 0x21 | 0x22 => {
                let i = r.u32()?;
                if i >= nlocals {
                    return err("unknown local");
                }
                match op {
                    0x20 => Op::LocalGet(i),
                    0x21 => Op::LocalSet(i),
                    _ => Op::LocalTee(i),
                }
            }
            0x23 | 0x24 => {
                let i = r.u32()?;
                if i as usize >= m.globals.len() {
                    return err("unknown global");
                }
                if op == 0x24 {
                    if !m.globals[i as usize].mutable {
                        return err("global is immutable");
                    }
                    Op::GlobalSet(i)
                } else {
                    Op::GlobalGet(i)
                }
            }
            0x28..=0x3E => {
                mem(m)?;
                let _align = r.u32()?;
                let offset = r.u32()?;
                if op <= 0x35 {
                    Op::Load { op, offset }
                } else {
                    Op::Store { op, offset }
                }
            }
            0x3F | 0x40 => {
                mem(m)?;
                if r.byte()? != 0 {
                    return err("zero byte expected");
                }
                if op == 0x3F {
                    Op::MemorySize
                } else {
                    Op::MemoryGrow
                }
            }
            0x41 => Op::I32Const(r.sleb(32)? as i32),
            0x42 => Op::I64Const(r.sleb(64)?),
            0x43 => Op::F32Const(u32::from_le_bytes(r.bytes(4)?.try_into().unwrap())),
            0x44 => Op::F64Const(u64::from_le_bytes(r.bytes(8)?.try_into().unwrap())),
            0x45..=0xC4 => Op::Num(op),
            0xFC => {
                let sub = r.u32()?;
                if sub > 7 {
                    return err("unsupported 0xFC instruction");
                }
                Op::TruncSat(sub as u8)
            }
            _ => return err(format!("illegal opcode 0x{:02x}", op)),
        };
        ops.push(o);
    }
    if r.p != code.len() {
        return err("operators remaining after end of function");
    }
    Ok((ops, tables))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_edges() {
        let mut r = Reader { b: &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F], p: 0 };
        assert_eq!(r.u32().unwrap(), u32::MAX);
        let mut r = Reader { b: &[0xFF, 0xFF, 0xFF, 0xFF, 0x1F], p: 0 };
        assert!(r.u32().is_err());
        let mut r = Reader { b: &[0x7F], p: 0 };
        assert_eq!(r.sleb(32).unwrap(), -1);
        let mut r = Reader { b: &[0x80, 0x80, 0x80, 0x80, 0x78], p: 0 };
        assert_eq!(r.sleb(32).unwrap(), i32::MIN as i64);
        let mut r = Reader { b: &[0x80, 0x80, 0x80, 0x80, 0x70], p: 0 };
        assert!(r.sleb(32).is_err());
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert!(decode(b"").is_err());
        assert!(decode(b"\0asm\x02\0\0\0").is_err());
        let mut x: u64 = 1;
        for len in 0..200 {
            let mut b = b"\0asm\x01\0\0\0".to_vec();
            for _ in 0..len {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b.push(x as u8);
            }
            let _ = decode(&b);
        }
    }
}
