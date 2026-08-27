// tig web frontend の JS glue。
//
// wasm 側 (web/src/lib.rs) との受け渡し規約:
// - 入力: tig_alloc で確保した領域へ書き込み (ptr, len) を渡し、tig_dealloc で解放
// - 出力: 「u32 (LE) 長 + データ」バッファ。読み取り後 tig_free で解放。
//   null (0) は失敗で、tig_last_error がメッセージを持つ

"use strict";

let wasm = null;

const utf8 = { enc: new TextEncoder(), dec: new TextDecoder() };

function mem() {
  return new Uint8Array(wasm.memory.buffer);
}

function toWasm(bytes) {
  const ptr = wasm.tig_alloc(bytes.length);
  mem().set(bytes, ptr);
  return ptr;
}

function readResult(ptr) {
  const len = new DataView(wasm.memory.buffer).getUint32(ptr, true);
  const out = mem().slice(ptr + 4, ptr + 4 + len);
  wasm.tig_free(ptr);
  return out;
}

function lastError() {
  return utf8.dec.decode(readResult(wasm.tig_last_error()));
}

function takeResult(ptr) {
  if (ptr === 0) throw new Error(lastError());
  return readResult(ptr);
}

function takeJson(ptr) {
  return JSON.parse(utf8.dec.decode(takeResult(ptr)));
}

function withBytes(bytes, f) {
  const ptr = toWasm(bytes);
  try {
    return f(ptr, bytes.length);
  } finally {
    wasm.tig_dealloc(ptr, bytes.length);
  }
}

// --- clone (sans-io driver を fetch() で駆動する) ---------------------------

async function cloneOverHttp(url, depth, refName) {
  const refBytes = utf8.enc.encode(refName);
  const handle = withBytes(refBytes, (p, n) => wasm.tig_clone_new(depth, p, n));
  try {
    for (;;) {
      const req = takeJson(wasm.tig_clone_next_json(handle));
      if (req.done) break;
      const body = takeResult(wasm.tig_clone_body(handle));
      const headers = { "Git-Protocol": "version=2" };
      if (req.method === "POST") {
        headers["Content-Type"] = "application/x-git-upload-pack-request";
      }
      const resp = await fetch(`${url.replace(/\/+$/, "")}/${req.path}`, {
        method: req.method,
        headers,
        body: req.method === "POST" ? body : undefined,
      });
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = new Uint8Array(await resp.arrayBuffer());
      const rc = withBytes(data, (p, n) => wasm.tig_clone_response(handle, p, n));
      if (rc !== 0) throw new Error(lastError());
    }
    return takeResult(wasm.tig_clone_finish_bundle(handle));
  } finally {
    wasm.tig_clone_close(handle);
  }
}

// --- UI ---------------------------------------------------------------------

const el = (id) => document.getElementById(id);
let bundleHandle = -1;
let bundleBytes = null;

function setStatus(text, isError = false) {
  el("status").textContent = text;
  el("status").className = isError ? "error" : "";
}

function openBundle(bytes, label) {
  const handle = withBytes(bytes, (p, n) => wasm.tig_bundle_open(p, n));
  if (handle < 0) {
    // 解析に失敗した場合は直前の bundle (handle / 表示 / download) を保持する。
    setStatus(`parse failed: ${lastError()}`, true);
    return;
  }
  if (bundleHandle >= 0) {
    wasm.tig_bundle_close(bundleHandle);
  }
  bundleHandle = handle;
  bundleBytes = bytes;
  setStatus(`${label} (${bytes.length.toLocaleString()} bytes)`);
  el("download").hidden = false;
  renderRefs();
  renderLog("");
}

function renderRefs() {
  const refs = takeJson(wasm.tig_refs_json(bundleHandle));
  const list = el("refs");
  list.replaceChildren();
  const all = document.createElement("li");
  all.textContent = "(all refs)";
  all.onclick = () => renderLog("");
  list.appendChild(all);
  for (const ref of refs) {
    const item = document.createElement("li");
    item.textContent = ref.name;
    item.title = ref.oid;
    item.onclick = () => renderLog(ref.name);
    list.appendChild(item);
  }
}

function renderLog(refName) {
  const refBytes = utf8.enc.encode(refName);
  let ptr;
  try {
    ptr = withBytes(refBytes, (p, n) => wasm.tig_log_json(bundleHandle, p, n, 1000));
    ptr = takeJson(ptr);
  } catch (e) {
    setStatus(e.message, true);
    return;
  }
  el("log-title").textContent = refName === "" ? "log: all refs" : `log: ${refName}`;
  const tbody = el("log");
  tbody.replaceChildren();
  for (const c of ptr) {
    const tr = document.createElement("tr");
    const date = new Date(c.time * 1000).toISOString().replace("T", " ").slice(0, 19);
    for (const text of [c.oid.slice(0, 12), date, c.author, c.subject]) {
      const td = document.createElement("td");
      td.textContent = text;
      tr.appendChild(td);
    }
    tr.onclick = () => showCommit(c.oid);
    tbody.appendChild(tr);
  }
}

function showCommit(oid) {
  const body = withBytes(utf8.enc.encode(oid), (p, n) =>
    takeResult(wasm.tig_cat(bundleHandle, p, n)),
  );
  el("detail-title").textContent = `commit ${oid}`;
  el("detail").textContent = utf8.dec.decode(body);
}

async function main() {
  const { instance } = await WebAssembly.instantiateStreaming(fetch("tig_web.wasm"), {});
  wasm = instance.exports;
  setStatus("ready");

  el("file").onchange = async (ev) => {
    const file = ev.target.files[0];
    if (!file) return;
    openBundle(new Uint8Array(await file.arrayBuffer()), file.name);
  };

  el("clone-form").onsubmit = async (ev) => {
    ev.preventDefault();
    const url = el("url").value.trim();
    const depth = Number(el("depth").value) || 0;
    const ref = el("ref").value.trim();
    setStatus("cloning...");
    try {
      const bytes = await cloneOverHttp(url, depth, ref);
      openBundle(bytes, `${url} (cloned)`);
    } catch (e) {
      setStatus(`clone failed: ${e.message}`, true);
    }
  };

  el("download").onclick = () => {
    if (!bundleBytes) return;
    const blob = new Blob([bundleBytes], { type: "application/octet-stream" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = "repo.bundle";
    a.click();
    URL.revokeObjectURL(a.href);
  };
}

main().catch((e) => setStatus(`init failed: ${e.message}`, true));
