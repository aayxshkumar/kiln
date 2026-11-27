//! kiln: a WebAssembly runtime checked against V8.
//!
//!     kiln run m.wasm export [i32:1 f64:0x... ...]   call one export
//!     kiln calls m.wasm calls.txt                    run a call script (one call per line)
//!     kiln validate m.wasm                           exit 0 if kiln accepts the module
//!     kiln gen SEED out.wasm out.calls               write a random valid program
//!     kiln examples DIR                              write the benchmark modules
//!     kiln bench m.wasm export arg                   time one call (ms, best of 3)

mod build;
mod examples;
mod exec;
mod gen;
mod module;

use exec::{format_value, Instance, InstantiateError};
use module::ValType;
use std::process::ExitCode;
use std::time::Instant;

fn parse_arg(s: &str) -> Result<(ValType, u64), String> {
    let (t, v) = s.split_once(':').ok_or_else(|| format!("bad argument {}", s))?;
    let hex = |v: &str| u64::from_str_radix(v.trim_start_matches("0x"), 16).map_err(|e| e.to_string());
    Ok(match t {
        "i32" => (ValType::I32, v.parse::<i32>().map_err(|e| e.to_string())? as u32 as u64),
        "i64" => (ValType::I64, v.parse::<i64>().map_err(|e| e.to_string())? as u64),
        "f32" => (ValType::F32, hex(v)?),
        "f64" => (ValType::F64, hex(v)?),
        _ => return Err(format!("bad argument type {}", t)),
    })
}

fn load(path: &str) -> Result<module::Module, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
    module::decode(&bytes).map_err(|e| format!("invalid module: {}", e))
}

fn call_line(inst: &mut Instance, name: &str, args: &[&str]) -> String {
    let Some(f) = inst.export(name) else { return format!("{} -> error:no such export", name) };
    let args: Result<Vec<(ValType, u64)>, String> = args.iter().map(|a| parse_arg(a)).collect();
    let args = match args {
        Ok(a) => a,
        Err(e) => return format!("{} -> error:{}", name, e),
    };
    let ty = inst.func_type(f).clone();
    if ty.params != args.iter().map(|a| a.0).collect::<Vec<_>>() {
        return format!("{} -> error:argument types", name);
    }
    let vals: Vec<u64> = args.iter().map(|a| a.1).collect();
    match inst.invoke(f, &vals) {
        Ok(rs) => {
            let out: Vec<String> = ty.results.iter().zip(rs).map(|(&t, v)| format_value(t, v)).collect();
            format!("{} -> {}", name, out.join(" "))
        }
        Err(t) => format!("{} -> trap:{}", name, t),
    }
}

fn instantiate(m: &module::Module) -> Result<Instance<'_>, String> {
    Instance::new(m).map_err(|e| match e {
        InstantiateError::Link(s) => format!("link:{}", s),
        InstantiateError::Trap(t) => format!("trap:{}", t),
    })
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().collect();
    let argv: Vec<&str> = a.iter().map(String::as_str).collect();
    let r: Result<(), String> = match argv.get(1..) {
        Some(["run", path, export, rest @ ..]) => load(path).and_then(|m| {
            let mut inst = instantiate(&m)?;
            println!("{}", call_line(&mut inst, export, rest));
            Ok(())
        }),
        Some(["calls", path, script]) => load(path).and_then(|m| {
            let text = std::fs::read_to_string(script).map_err(|e| e.to_string())?;
            let mut inst = instantiate(&m)?;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let mut parts = line.split_whitespace();
                let name = parts.next().unwrap();
                let args: Vec<&str> = parts.collect();
                println!("{}", call_line(&mut inst, name, &args));
            }
            Ok(())
        }),
        Some(["validate", path]) => load(path).map(|_| ()),
        Some(["gen", seed, wasm, calls]) => seed.parse::<u64>().map_err(|e| e.to_string()).and_then(|s| {
            let (bytes, script) = gen::module(s);
            std::fs::write(wasm, bytes).map_err(|e| e.to_string())?;
            std::fs::write(calls, script).map_err(|e| e.to_string())
        }),
        Some(["examples", dir]) => {
            let w = |n: &str, b: Vec<u8>| std::fs::write(std::path::Path::new(dir).join(n), b).map_err(|e| e.to_string());
            w("fib.wasm", examples::fib()).and(w("sieve.wasm", examples::sieve())).and(w("collatz.wasm", examples::collatz()))
        }
        Some(["bench", path, export, arg]) => load(path).and_then(|m| {
            let mut best = f64::MAX;
            let mut line = String::new();
            for _ in 0..3 {
                let mut inst = instantiate(&m)?;
                let t = Instant::now();
                line = call_line(&mut inst, export, &[arg]);
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            println!("{} ({:.1} ms)", line, best);
            Ok(())
        }),
        _ => Err("usage: kiln run|calls|validate|gen|examples|bench (see src/main.rs)".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kiln: {}", e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bytes: Vec<u8>, export: &str, args: &[&str]) -> String {
        let m = module::decode(&bytes).unwrap();
        let mut inst = Instance::new(&m).unwrap();
        call_line(&mut inst, export, args)
    }

    #[test]
    fn examples_compute_known_answers() {
        assert_eq!(run(examples::fib(), "fib", &["i32:20"]), "fib -> i32:6765");
        assert_eq!(run(examples::sieve(), "sieve", &["i32:100"]), "sieve -> i32:25");
        assert_eq!(run(examples::collatz(), "collatz", &["i32:10"]), "collatz -> i64:67");
    }

    #[test]
    fn generated_modules_decode_and_run() {
        for seed in 0..200 {
            let (bytes, script) = gen::module(seed);
            let m = module::decode(&bytes).unwrap_or_else(|e| panic!("seed {}: {}", seed, e));
            let mut inst = Instance::new(&m).unwrap();
            for line in script.lines() {
                let mut p = line.split_whitespace();
                let name = p.next().unwrap();
                let args: Vec<&str> = p.collect();
                let out = call_line(&mut inst, name, &args);
                assert!(!out.contains("error"), "seed {}: {}", seed, out);
            }
        }
    }

    #[test]
    fn traps() {
        use build::{Builder, Code, Func};
        use module::ValType::*;
        let mut b = Builder { memory: Some((1, None)), ..Default::default() };
        let mut div = Code::default();
        div.get(0).get(1).op(0x6D);
        b.funcs.push(Func { params: vec![I32, I32], results: vec![I32], locals: vec![], code: div });
        let mut ld = Code::default();
        ld.get(0).mem(0x28, 0);
        b.funcs.push(Func { params: vec![I32], results: vec![I32], locals: vec![], code: ld });
        let mut tr = Code::default();
        tr.get(0).op(0xAA);
        b.funcs.push(Func { params: vec![F64], results: vec![I32], locals: vec![], code: tr });
        b.exports = vec![("div".into(), 0), ("load".into(), 1), ("trunc".into(), 2)];
        let bytes = b.encode();
        assert_eq!(run(bytes.clone(), "div", &["i32:7", "i32:0"]), "div -> trap:div-zero");
        assert_eq!(run(bytes.clone(), "div", &["i32:-2147483648", "i32:-1"]), "div -> trap:overflow");
        assert_eq!(run(bytes.clone(), "div", &["i32:-7", "i32:2"]), "div -> i32:-3");
        assert_eq!(run(bytes.clone(), "load", &["i32:65532"]), "load -> i32:0");
        assert_eq!(run(bytes.clone(), "load", &["i32:65533"]), "load -> trap:oob");
        assert_eq!(run(bytes.clone(), "trunc", &["f64:0x41dfffffffc00000"]), "trunc -> i32:2147483647");
        assert_eq!(run(bytes, "trunc", &["f64:0x41e0000000000000"]), "trunc -> trap:bad-conversion");
    }
}
