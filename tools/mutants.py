"""Plant known bugs one at a time and report what catches each.

Each mutant is one text replacement in src/. For each, this builds kiln into
a scratch target dir, runs the unit tests and check.py (without the
benchmarks), restores the file, and reports what failed. A mutant nothing
catches is a hole in the tests.

    python3 tools/mutants.py
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

MUTANTS = [
    ("shr-s-unsigned", "src/exec.rs", "0x75 => ai.wrapping_shr(b) as u32,", "0x75 => a.wrapping_shr(b),", "i32.shr_s shifts in zeros"),
    ("rotl-is-rotr", "src/exec.rs", "0x77 => a.rotate_left(b),", "0x77 => a.rotate_right(b),", "i32.rotl rotates right"),
    ("fmin-zero-sign", "src/exec.rs", "return a.to_bits() | b.to_bits();\n    }\n    (if a < b { a } else { b }).to_bits()\n}\n\nfn fmax32",
     "return a.to_bits();\n    }\n    (if a < b { a } else { b }).to_bits()\n}\n\nfn fmax32", "f32.min(0, -0) can return +0"),
    ("nearest-away", "src/exec.rs", "0x9E => a.round_ties_even().to_bits(),", "0x9E => a.round().to_bits(),", "f64.nearest rounds halves away from zero"),
    ("trunc-u-accepts-minus-one", "src/exec.rs", "0xA9 | 0xAB => (0.0, 4294967296.0),", "0xA9 | 0xAB => (-1.0, 4294967296.0),", "i32.trunc_f*_u accepts -1"),
    ("load8-unsigned", "src/exec.rs", "0x2C => raw as u8 as i8 as i32 as u32 as u64,", "0x2C => raw,", "i32.load8_s zero-extends"),
    ("store16-writes-4", "src/exec.rs", "            0x3A | 0x3C => 1,\n            _ => 2,", "            0x3A | 0x3C => 1,\n            _ => 4,", "16-bit stores write 4 bytes"),
    ("br-if-inverted", "src/exec.rs", "Op::BrIf(d) => {\n                    if pop!() as u32 != 0 {", "Op::BrIf(d) => {\n                    if pop!() as u32 == 0 {", "br_if branches on zero"),
    ("select-swapped", "src/exec.rs", "push!(if c != 0 { a } else { b });", "push!(if c != 0 { b } else { a });", "select picks the wrong operand"),
    ("grow-ignores-max", "src/exec.rs", "> self.max_pages as u64 {", "> 65536 {", "memory.grow ignores the declared maximum"),
    ("clz64-is-ctz", "src/exec.rs", "0x79 => a.leading_zeros(),", "0x79 => a.trailing_zeros(),", "i64.clz counts trailing zeros"),
    ("extend16-as-8", "src/exec.rs", "0xC1 => pop!() as u16 as i16 as i32 as u32 as u64,", "0xC1 => pop!() as u8 as i8 as i32 as u32 as u64,", "i32.extend16_s extends from 8 bits"),
    ("eqz64-low-bits", "src/exec.rs", "0x50 => b2(pop!() == 0),", "0x50 => b2(pop!() as u32 == 0),", "i64.eqz looks at the low 32 bits"),
    ("copysign-noop", "src/exec.rs", "_ => (ab & 0x7FFF_FFFF) | (bb & 0x8000_0000),", "_ => ab,", "f32.copysign returns its first operand"),
    ("oob-off-by-one", "src/exec.rs", "if a + n > self.memory.len() as u64 {", "if a + n > self.memory.len() as u64 + 1 {", "an access one byte past the end is allowed (and panics)"),
]


def run(cmd):
    return subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, timeout=1200)


def main():
    t0 = time.time()
    target = tempfile.mkdtemp(prefix="kiln-mutants-")
    rows, caught = [], 0
    try:
        for name, path, old, new, what in MUTANTS:
            full = os.path.join(ROOT, path)
            orig = open(full).read()
            if orig.count(old) != 1:
                rows.append((name, "BROKEN", "pattern matches %d times" % orig.count(old)))
                continue
            try:
                open(full, "w").write(orig.replace(old, new))
                if run(["cargo", "build", "--release", "-q", "--target-dir", target]).returncode:
                    rows.append((name, "BROKEN", "does not compile"))
                    continue
                t = run(["cargo", "test", "--release", "-q", "--target-dir", target])
                c = run(["python3", "tools/check.py", "--no-bench", "--kiln", os.path.join(target, "release", "kiln")])
                by = [x for x, r in (("tests", t), ("check", c)) if r.returncode]
            finally:
                open(full, "w").write(orig)
            caught += bool(by)
            rows.append((name, "caught" if by else "SURVIVED", ", ".join(by) or what))
    finally:
        shutil.rmtree(target, ignore_errors=True)
    for r in rows:
        print("%-28s %-9s %s" % r)
    survived = [n for n, res, _ in rows if res != "caught"]
    print("\n%d/%d caught%s (%.0f s)" % (caught, len(MUTANTS), "; survived: " + ", ".join(survived) if survived else "", time.time() - t0))
    return 0


if __name__ == "__main__":
    sys.exit(main())
