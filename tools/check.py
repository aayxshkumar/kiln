"""Check kiln against V8 (the engine inside Node).

1. differential: random valid programs (kiln gen) run in kiln and in V8 with
   the same calls; every result, trap and memory checksum must match.
2. validation: those programs with one or two bytes flipped; kiln must accept
   exactly the modules V8 accepts, and never crash.
3. features: small modules that use standard features beyond the MVP; V8
   runs them all.
4. speed: the benchmark programs, kiln's time over V8's.

    python3 tools/check.py [--modules 300] [--node node]
"""
import argparse
import json
import math
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCHES = [("fib.wasm", "fib", "i32:30"), ("sieve.wasm", "sieve", "i32:5000000"), ("collatz.wasm", "collatz", "i32:300000")]
SPEED_GOAL = 2.0  # within 2x of V8 is the long-term bar


def uleb(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)


def sec(i, body):
    return bytes([i]) + uleb(len(body)) + body


def vec(items):
    return uleb(len(items)) + b"".join(items)


def module(types, funcs, extra=b"", imports=None, memory=False):
    """types: list of (params bytes, results bytes); funcs: list of (type, body bytes)."""
    m = b"\0asm\x01\0\0\0"
    m += sec(1, vec([b"\x60" + uleb(len(p)) + p + uleb(len(r)) + r for p, r in types]))
    if imports:
        m += sec(2, vec(imports))
    m += sec(3, vec([uleb(t) for t, _ in funcs]))
    if memory:
        m += sec(5, b"\x01\x00\x01")
    m += extra
    m += sec(10, vec([uleb(len(b"\x00" + body + b"\x0b")) + b"\x00" + body + b"\x0b" for _, body in funcs]))
    return m


def name(s):
    return uleb(len(s)) + s.encode()


FEATURES = {
    "imports": module([(b"", b"")], [(0, b"\x10\x00")], imports=[name("env") + name("f") + b"\x00\x00"]),
    "multi-value": module([(b"", b"\x7f\x7f")], [(0, b"\x41\x01\x41\x02")]),
    "bulk-memory": module([(b"", b"")], [(0, b"\x41\x00\x41\x00\x41\x00\xfc\x0a\x00\x00")], memory=True),
    "reference-types": module([(b"", b"")], [(0, b"\xd0\x70\x1a")]),
    "simd": module([(b"", b"")], [(0, b"\xfd\x0c" + bytes(16) + b"\x1a")]),
    "tail-calls": module([(b"", b"")], [(0, b"\x12\x00")]),
}


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--modules", type=int, default=300)
    ap.add_argument("--node", default="node")
    ap.add_argument("--kiln", default=os.path.join(ROOT, "target", "release", "kiln"))
    ap.add_argument("--no-bench", action="store_true")
    ap.add_argument("-v", "--verbose", action="store_true")
    args = ap.parse_args()
    t0 = time.time()
    kiln, node = args.kiln, args.node
    runmjs = os.path.join(ROOT, "tools", "run.mjs")
    if not os.path.exists(kiln):
        sys.exit("check.py: build first: cargo build --release")
    tmp = tempfile.mkdtemp(prefix="kiln-check-")
    rows = []
    failed = gaps = 0
    try:
        # 1. differential
        gen = os.path.join(tmp, "gen")
        os.mkdir(gen)
        for s in range(1, args.modules + 1):
            base = os.path.join(gen, "m%04d" % s)
            r = run([kiln, "gen", str(s), base + ".wasm", base + ".calls"])
            if r.returncode:
                sys.exit("kiln gen %d failed: %s" % (s, r.stderr))
        v8 = json.loads(run([node, runmjs, "calls", gen], check=True).stdout)
        calls = mismatched = traps = 0
        bad_modules = []
        for f in sorted(v8):
            base = os.path.join(gen, f[:-5])
            r = run([kiln, "calls", base + ".wasm", base + ".calls"])
            ours = r.stdout.splitlines() if r.returncode == 0 else ["error:" + r.stderr.strip()]
            theirs = v8[f]
            calls += len(theirs)
            traps += sum("trap:" in x for x in theirs)
            diff = [(a, b) for a, b in zip(ours, theirs) if a != b] + ([("<missing>", "")] if len(ours) != len(theirs) else [])
            if diff:
                mismatched += 1
                bad_modules.append(f)
                if args.verbose or mismatched <= 5:
                    print("  %s: kiln %r, V8 %r" % (f, diff[0][0], diff[0][1]))
        res = "PASS" if mismatched == 0 else "FAIL"
        failed += res == "FAIL"
        rows.append(("differential", res, "%d" % mismatched, "0",
                     "random modules whose results differ from V8 (of %d; %d calls, %d of them traps)" % (len(v8), calls, traps)))

        # 2. validation of corrupted modules
        bad = os.path.join(tmp, "bad")
        os.mkdir(bad)
        rnd = random.Random(1)
        for s in range(1, args.modules + 1):
            data = bytearray(open(os.path.join(gen, "m%04d.wasm" % s), "rb").read())
            for _ in range(rnd.choice([1, 1, 2])):
                i = rnd.randrange(8, len(data))
                data[i] = rnd.randrange(256)
            open(os.path.join(bad, "c%04d.wasm" % s), "wb").write(data)
        v8ok = json.loads(run([node, runmjs, "validate", bad], check=True).stdout)
        wrongly_ok = wrongly_bad = crashes = 0
        for f, ok in sorted(v8ok.items()):
            r = run([kiln, "validate", os.path.join(bad, f)])
            if r.returncode not in (0, 1):
                crashes += 1
                print("  %s: kiln crashed: %s" % (f, r.stderr.strip()[-200:]))
                continue
            mine = r.returncode == 0
            if mine and not ok:
                wrongly_ok += 1
            elif ok and not mine:
                wrongly_bad += 1
                if args.verbose:
                    print("  %s: V8 accepts, kiln rejects: %s" % (f, r.stderr.strip()))
        res = "PASS" if crashes == 0 else "FAIL"
        failed += res == "FAIL"
        rows.append(("no-crash", res, "%d" % crashes, "0", "corrupted modules that crash kiln (of %d)" % len(v8ok)))
        v8_rejects = sum(not ok for ok in v8ok.values())
        frac = wrongly_ok / v8_rejects if v8_rejects else 0.0
        res = "PASS" if wrongly_ok == 0 and wrongly_bad == 0 else "KNOWN GAP"
        gaps += res == "KNOWN GAP"
        rows.append(("validation", res, "%.3f" % frac, "0",
                     "of the %d corrupted modules V8 rejects, fraction kiln accepts (and %d V8 accepts that kiln rejects)" % (v8_rejects, wrongly_bad)))

        # 3. features beyond the MVP
        feat = os.path.join(tmp, "feat")
        os.mkdir(feat)
        for n, b in FEATURES.items():
            open(os.path.join(feat, n + ".wasm"), "wb").write(b)
        v8feat = json.loads(run([node, runmjs, "validate", feat], check=True).stdout)
        missing = [n for n in FEATURES if v8feat[n + ".wasm"] and run([kiln, "validate", os.path.join(feat, n + ".wasm")]).returncode != 0]
        res = "PASS" if not missing else "KNOWN GAP"
        gaps += res == "KNOWN GAP"
        rows.append(("features", res, "%d" % len(missing), "0", "standard features V8 supports and kiln rejects: " + ", ".join(missing)))

        # 4. speed
        if not args.no_bench:
            ex = os.path.join(tmp, "ex")
            os.mkdir(ex)
            run([kiln, "examples", ex], check=True)
            ratios = []
            for f, export, arg in BENCHES:
                ko = run([kiln, "bench", os.path.join(ex, f), export, arg], check=True).stdout.strip()
                vo = run([node, runmjs, "bench", os.path.join(ex, f), export, arg], check=True).stdout.strip()
                kres, kms = ko.rsplit(" (", 1)
                vres, vms = vo.rsplit(" (", 1)
                kms, vms = float(kms.split()[0]), float(vms.split()[0])
                if kres != vres:
                    failed += 1
                    print("  %s: kiln %r, V8 %r" % (f, kres, vres))
                ratio = kms / max(vms, 0.05)
                ratios.append(ratio)
                print("  bench %-14s kiln %8.1f ms   V8 %7.1f ms   %5.1fx" % (export + "(" + arg.split(":")[1] + ")", kms, vms, ratio))
            geo = math.exp(sum(math.log(r) for r in ratios) / len(ratios))
            res = "PASS" if geo <= SPEED_GOAL else "KNOWN GAP"
            gaps += res == "KNOWN GAP"
            rows.append(("speed", res, "%.1f" % geo, "%.1f" % SPEED_GOAL, "kiln time over V8 time, geometric mean of %d benchmarks" % len(ratios)))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print("%-13s %-10s %7s %9s  %s" % ("check", "result", "value", "threshold", "measures"))
    for r in rows:
        print("%-13s %-10s %7s %9s  %s" % r)
    print("\n%d failed, %d known gaps (%.1f s)" % (failed, gaps, time.time() - t0))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
