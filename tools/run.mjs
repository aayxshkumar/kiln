// Run kiln's call scripts under V8 (Node) and print results in kiln's format.
//
//   node tools/run.mjs calls DIR          every DIR/*.wasm with its .calls script
//   node tools/run.mjs validate DIR       WebAssembly.validate on every DIR/*.wasm
//   node tools/run.mjs bench m.wasm export arg
import fs from 'node:fs';
import path from 'node:path';

const buf = new ArrayBuffer(8);
const dv = new DataView(buf);

function toJs(arg) {
  const [t, v] = arg.split(':');
  if (t === 'i32') return Number(v) | 0;
  if (t === 'i64') return BigInt(v);
  if (t === 'f32') { dv.setUint32(0, parseInt(v, 16)); return dv.getFloat32(0); }
  dv.setBigUint64(0, BigInt(v)); return dv.getFloat64(0);
}

function fmt(type, v) {
  if (type === 'i32') return 'i32:' + (v | 0);
  if (type === 'i64') return 'i64:' + BigInt.asIntN(64, v);
  if (Number.isNaN(v)) return type + ':nan';
  if (type === 'f32') { dv.setFloat32(0, v); return 'f32:0x' + dv.getUint32(0).toString(16).padStart(8, '0'); }
  dv.setFloat64(0, v); return 'f64:0x' + dv.getBigUint64(0).toString(16).padStart(16, '0');
}

function trapKind(e) {
  const m = String(e && e.message);
  if (e instanceof RangeError && /call stack/i.test(m)) return 'stack';
  if (/unreachable/.test(m)) return 'unreachable';
  if (/by zero/.test(m)) return 'div-zero';
  if (/divide result unrepresentable/.test(m)) return 'overflow';
  if (/out of bounds/.test(m) && /memory/.test(m)) return 'oob';
  if (/float unrepresentable/.test(m)) return 'bad-conversion';
  if (/table|signature|null function/.test(m)) return 'indirect';
  return 'other(' + m + ')';
}

// result types per export, read from the binary's type and function sections
function resultTypes(bytes) {
  const mod = new WebAssembly.Module(bytes);
  const names = WebAssembly.Module.exports(mod).filter(e => e.kind === 'function').map(e => e.name);
  let p = 8; const types = []; let funcTypes = []; const exportsIdx = {};
  const u = () => { let r = 0, s = 0, b; do { b = bytes[p++]; r |= (b & 0x7f) << s; s += 7; } while (b & 0x80); return r >>> 0; };
  const vt = { 0x7f: 'i32', 0x7e: 'i64', 0x7d: 'f32', 0x7c: 'f64' };
  while (p < bytes.length) {
    const id = bytes[p++]; const size = u(); const end = p + size;
    if (id === 1) { const n = u(); for (let i = 0; i < n; i++) { p++; const np = u(); p += np; const nr = u(); const rs = []; for (let j = 0; j < nr; j++) rs.push(vt[bytes[p++]]); types.push(rs); } }
    else if (id === 3) { const n = u(); for (let i = 0; i < n; i++) funcTypes.push(u()); }
    else if (id === 7) { const n = u(); for (let i = 0; i < n; i++) { const l = u(); const name = Buffer.from(bytes.slice(p, p + l)).toString(); p += l; const kind = bytes[p++]; const idx = u(); if (kind === 0) exportsIdx[name] = idx; } }
    p = end;
  }
  const out = {};
  for (const n of names) out[n] = types[funcTypes[exportsIdx[n]]];
  return [mod, out];
}

function runCalls(wasmPath) {
  const bytes = new Uint8Array(fs.readFileSync(wasmPath));
  const script = fs.readFileSync(wasmPath.replace(/\.wasm$/, '.calls'), 'utf8');
  const lines = [];
  let inst, results;
  try {
    const [mod, rt] = resultTypes(bytes);
    results = rt;
    inst = new WebAssembly.Instance(mod, {});
  } catch (e) {
    return ['error:' + e.message];
  }
  for (const line of script.split('\n').filter(l => l.trim())) {
    const [name, ...args] = line.trim().split(/\s+/);
    try {
      const v = inst.exports[name](...args.map(toJs));
      const rs = results[name];
      lines.push(name + ' -> ' + (rs.length ? fmt(rs[0], v) : ''));
    } catch (e) {
      if (e instanceof WebAssembly.RuntimeError || e instanceof RangeError) lines.push(name + ' -> trap:' + trapKind(e));
      else throw e;
    }
  }
  return lines;
}

const [cmd, ...rest] = process.argv.slice(2);
if (cmd === 'calls') {
  const dir = rest[0]; const out = {};
  for (const f of fs.readdirSync(dir).filter(f => f.endsWith('.wasm')).sort()) out[f] = runCalls(path.join(dir, f));
  process.stdout.write(JSON.stringify(out));
} else if (cmd === 'validate') {
  const dir = rest[0]; const out = {};
  for (const f of fs.readdirSync(dir).filter(f => f.endsWith('.wasm')).sort()) out[f] = WebAssembly.validate(new Uint8Array(fs.readFileSync(path.join(dir, f))));
  process.stdout.write(JSON.stringify(out));
} else if (cmd === 'bench') {
  const [file, name, arg] = rest;
  const bytes = new Uint8Array(fs.readFileSync(file));
  let best = Infinity, line = '';
  const [mod, rt] = resultTypes(bytes);
  for (let i = 0; i < 3; i++) {
    const inst = new WebAssembly.Instance(mod, {});
    const t = performance.now();
    const v = inst.exports[name](toJs(arg));
    best = Math.min(best, performance.now() - t);
    line = name + ' -> ' + fmt(rt[name][0], v);
  }
  console.log(line + ' (' + best.toFixed(1) + ' ms)');
} else {
  console.error('usage: node tools/run.mjs calls DIR | validate DIR | bench m.wasm export arg');
  process.exit(2);
}
