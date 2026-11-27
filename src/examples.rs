//! Benchmark programs written with the encoder: recursion, memory, i64 loops.

use crate::build::{Builder, Code, Func};
use crate::module::ValType::*;

pub fn fib() -> Vec<u8> {
    let mut c = Code::default();
    c.get(0).i32c(2).op(0x48).block(0x04, Some(I32)).get(0).op(0x05);
    c.get(0).i32c(1).op(0x6B).op(0x10).u(0).get(0).i32c(2).op(0x6B).op(0x10).u(0).op(0x6A).op(0x0B);
    let mut b = Builder::default();
    b.funcs.push(Func { params: vec![I32], results: vec![I32], locals: vec![], code: c });
    b.exports.push(("fib".into(), 0));
    b.encode()
}

pub fn sieve() -> Vec<u8> {
    // count primes below n with a byte-per-number sieve in linear memory
    let mut c = Code::default();
    c.i32c(2).set(1);
    c.block(0x02, None).block(0x03, None);
    c.get(1).get(0).op(0x4F).op(0x0D).u(1);
    c.get(1).mem(0x2D, 0).op(0x45).block(0x04, None);
    c.get(3).i32c(1).op(0x6A).set(3);
    c.get(1).i32c(46341).op(0x49).block(0x04, None);
    c.get(1).get(1).op(0x6C).set(2);
    c.block(0x02, None).block(0x03, None);
    c.get(2).get(0).op(0x4F).op(0x0D).u(1);
    c.get(2).i32c(1).mem(0x3A, 0);
    c.get(2).get(1).op(0x6A).set(2);
    c.op(0x0C).u(0).op(0x0B).op(0x0B);
    c.op(0x0B).op(0x0B);
    c.get(1).i32c(1).op(0x6A).set(1);
    c.op(0x0C).u(0).op(0x0B).op(0x0B);
    c.get(3);
    let mut b = Builder { memory: Some((80, None)), ..Default::default() };
    b.funcs.push(Func { params: vec![I32], results: vec![I32], locals: vec![I32, I32, I32], code: c });
    b.exports.push(("sieve".into(), 0));
    b.encode()
}

pub fn collatz() -> Vec<u8> {
    // total Collatz steps for every k in 1..=n, in i64
    let mut c = Code::default();
    c.i64c(1).set(1);
    c.block(0x02, None).block(0x03, None);
    c.get(1).get(0).op(0xAD).op(0x56).op(0x0D).u(1);
    c.get(1).set(2);
    c.block(0x02, None).block(0x03, None);
    c.get(2).i64c(1).op(0x58).op(0x0D).u(1);
    c.get(2).i64c(1).op(0x83).op(0x50).block(0x04, Some(I64));
    c.get(2).i64c(1).op(0x88).op(0x05).get(2).i64c(3).op(0x7E).i64c(1).op(0x7C).op(0x0B).set(2);
    c.get(3).i64c(1).op(0x7C).set(3);
    c.op(0x0C).u(0).op(0x0B).op(0x0B);
    c.get(1).i64c(1).op(0x7C).set(1);
    c.op(0x0C).u(0).op(0x0B).op(0x0B);
    c.get(3);
    let mut b = Builder::default();
    b.funcs.push(Func { params: vec![I32], results: vec![I64], locals: vec![I64, I64, I64], code: c });
    b.exports.push(("collatz".into(), 0));
    b.encode()
}
